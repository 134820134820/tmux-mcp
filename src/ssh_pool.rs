//! Persistent SSH sessions for short remote tmux/file requests.
//!
//! A fresh `ssh` per request pays the whole handshake (3.5–8 s measured to the lab hosts,
//! for ~0.5 s of remote work). A session keeps one `ssh` running a small bash dispatcher:
//! each request is one line in and one line out, so a reused session costs one round trip.
//!
//! Sessions are keyed by the full ssh argv, so different targets/accounts never share one.
//! Each request runs through the login shell like a one-shot `ssh host cmd`, with stdin,
//! stdout and stderr redirected to private temp files so it can never read or corrupt the
//! request stream. A host without bash/base64/mktemp reports itself unsupported and the
//! caller falls back to one-shot `ssh`; `TMUX_MCP_SSH_POOL=0` disables sessions entirely.

use std::collections::HashMap;
use std::process::Stdio;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use base64::engine::general_purpose::STANDARD;
use base64::Engine;
use once_cell::sync::Lazy;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStdin, ChildStdout};
use uuid::Uuid;

/// Budget for ssh setup and the dispatcher banner; separate from request timeouts.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(30);
const PING_TIMEOUT: Duration = Duration::from_secs(5);
/// A modifying request first pings a session idle this long, so a silently dropped
/// connection is replaced before the request bytes are sent.
const PING_AFTER_IDLE: Duration = Duration::from_secs(20);
const IDLE_EXPIRY: Duration = Duration::from_secs(600);
const MAX_IDLE_PER_KEY: usize = 4;
const UNSUPPORTED_RETRY_AFTER: Duration = Duration::from_secs(600);
/// Output lines a login shell may print before the banner (motd, rc-file chatter).
const MAX_PREAMBLE_LINES: usize = 200;
const STDERR_TAIL_BYTES: usize = 4096;
const SSH_OPTIONS: [&str; 4] = [
    "-o",
    "ServerAliveInterval=15",
    "-o",
    "ServerAliveCountMax=3",
];

/// Remote dispatcher. `$1` is a per-session nonce echoed in the ready banner.
/// No locale is forced: requests must see the same environment as one-shot `ssh`.
const DISPATCHER: &str = r#"nonce=$1
for t in base64 head wc mktemp cat; do command -v "$t" >/dev/null 2>&1 || exit 98; done
[ "$(printf x | base64 -w0 2>/dev/null)" = eA== ] || exit 98
[ "$(printf eA== | base64 -d 2>/dev/null)" = x ] || exit 98
d=$(mktemp -d "${TMPDIR:-/tmp}/tmux-mcp-pool.XXXXXX") || exit 97
trap 'rm -rf "$d"' EXIT
trap 'exit 0' HUP INT TERM PIPE
run=${SHELL:-/bin/sh}
printf 'TMUX_MCP_POOL 1 %s\n' "$nonce"
while IFS=' ' read -r op id cap cmd input; do
  case $op in
    P) printf 'P %s\n' "$id" ;;
    R)
      if ! printf '%s' "$cmd" | base64 -d > "$d/c" 2>/dev/null; then printf 'X %s bad-request\n' "$id"; continue; fi
      if [ "$input" = - ]; then : > "$d/i"
      elif ! printf '%s' "$input" | base64 -d > "$d/i" 2>/dev/null; then printf 'X %s bad-request\n' "$id"; continue; fi
      "$run" -c "$(cat "$d/c")" < "$d/i" > "$d/o" 2> "$d/e"
      rc=$?
      n=$(( $(wc -c < "$d/o") ))
      o=$(head -c "$cap" "$d/o" | base64 -w0)
      e=$(head -c 65536 "$d/e" | base64 -w0)
      printf 'D %s %s %s %s %s\n' "$id" "$rc" "$n" "${o:--}" "${e:--}"
      ;;
    *) printf 'X %s unknown\n' "$id" ;;
  esac
done"#;

pub struct Request<'a> {
    pub command: &'a str,
    pub stdin: Option<&'a [u8]>,
    /// Stdout bytes returned; `stdout_truncated` reports whether more existed.
    pub stdout_cap: usize,
    /// Remote work budget, excluding connection setup.
    pub timeout: Duration,
    /// Safe to resend after a reused session turns out to be dead.
    pub idempotent: bool,
}

#[derive(Debug)]
pub struct RemoteOutput {
    pub code: i32,
    pub stdout: Vec<u8>,
    pub stdout_truncated: bool,
    pub stderr: Vec<u8>,
}

#[derive(Debug)]
pub enum Outcome {
    Done {
        output: RemoteOutput,
        reused: bool,
    },
    /// The host cannot run the dispatcher; use one-shot `ssh`.
    Unsupported,
}

#[derive(Debug)]
pub enum Failure {
    /// The request was sent but no answer arrived within its budget.
    Timeout,
    /// The session died; the request may or may not have run remotely.
    Lost(String),
    /// No session could be established; nothing was sent.
    Connect(String),
}

#[derive(Default)]
struct KeyState {
    idle: Vec<Session>,
    unsupported_until: Option<Instant>,
}

static POOL: Lazy<Mutex<HashMap<Vec<String>, KeyState>>> = Lazy::new(Default::default);
static NEXT_ID: AtomicU64 = AtomicU64::new(1);

pub fn enabled() -> bool {
    !matches!(
        std::env::var("TMUX_MCP_SSH_POOL").as_deref(),
        Ok("0" | "off" | "false" | "no")
    )
}

/// Close every pooled session (tests use separate runtimes per case).
#[cfg(test)]
pub fn clear() {
    POOL.lock().unwrap_or_else(|e| e.into_inner()).clear();
}

/// Sessions are bound to the tokio runtime that spawned them. Production has one runtime;
/// each `#[tokio::test]` has its own on its own thread, so tests key by thread as well.
fn pool_key(ssh_args: &[String]) -> Vec<String> {
    #[allow(unused_mut)]
    let mut key = ssh_args.to_vec();
    #[cfg(test)]
    key.push(format!("{:?}", std::thread::current().id()));
    key
}

pub async fn run(ssh_args: &[String], request: Request<'_>) -> Result<Outcome, Failure> {
    let key = pool_key(ssh_args);
    if unsupported(&key) {
        return Ok(Outcome::Unsupported);
    }
    let id = NEXT_ID.fetch_add(1, Ordering::Relaxed);
    let line = format!(
        "R {id} {} {} {}\n",
        request.stdout_cap,
        STANDARD.encode(request.command),
        request
            .stdin
            .filter(|input| !input.is_empty())
            .map_or_else(|| "-".to_string(), |input| STANDARD.encode(input)),
    );
    let mut retried = false;
    loop {
        let (mut session, reused) = match checkout(&key, request.idempotent).await {
            Some(session) => (session, true),
            None => match connect(ssh_args).await? {
                Some(session) => (session, false),
                None => {
                    mark_unsupported(&key);
                    return Ok(Outcome::Unsupported);
                }
            },
        };
        match session
            .exchange(&line, id, request.stdout_cap, request.timeout)
            .await
        {
            Ok(output) => {
                checkin(&key, session);
                return Ok(Outcome::Done { output, reused });
            }
            // A reused session can die while idle (NAT, server restart). Only a read can be
            // resent safely; a modifying request was pinged first and is never repeated.
            Err(Failure::Lost(reason)) if reused && request.idempotent && !retried => {
                tracing::debug!(%reason, "pooled ssh session lost; retrying read on a new one");
                retried = true;
            }
            Err(failure) => return Err(failure),
        }
    }
}

fn unsupported(key: &[String]) -> bool {
    let mut pool = POOL.lock().unwrap_or_else(|e| e.into_inner());
    let Some(state) = pool.get_mut(key) else {
        return false;
    };
    match state.unsupported_until {
        Some(until) if Instant::now() < until => true,
        Some(_) => {
            state.unsupported_until = None;
            false
        }
        None => false,
    }
}

fn mark_unsupported(key: &[String]) {
    tracing::warn!("remote host cannot run the ssh session dispatcher; using one-shot ssh");
    POOL.lock()
        .unwrap_or_else(|e| e.into_inner())
        .entry(key.to_vec())
        .or_default()
        .unsupported_until = Some(Instant::now() + UNSUPPORTED_RETRY_AFTER);
}

async fn checkout(key: &[String], idempotent: bool) -> Option<Session> {
    loop {
        let mut session = {
            let mut pool = POOL.lock().unwrap_or_else(|e| e.into_inner());
            pool.get_mut(key)?.idle.pop()?
        };
        if !session.alive() || session.last_used.elapsed() > IDLE_EXPIRY {
            continue;
        }
        if !idempotent && session.last_used.elapsed() > PING_AFTER_IDLE && !session.ping().await {
            continue;
        }
        return Some(session);
    }
}

fn checkin(key: &[String], mut session: Session) {
    session.last_used = Instant::now();
    let mut pool = POOL.lock().unwrap_or_else(|e| e.into_inner());
    let state = pool.entry(key.to_vec()).or_default();
    if state.idle.len() < MAX_IDLE_PER_KEY {
        state.idle.push(session);
    }
}

fn bootstrap_command(nonce: &str) -> String {
    format!(
        "exec bash -c {} tmux-mcp-pool {nonce}",
        crate::tmux::quote_remote_arg(DISPATCHER)
    )
}

/// Start one session. `Ok(None)` means the host answered but cannot run the dispatcher.
async fn connect(ssh_args: &[String]) -> Result<Option<Session>, Failure> {
    let nonce = Uuid::new_v4().simple().to_string();
    let mut command = crate::tmux::external("ssh");
    command
        .arg("-T")
        .args(SSH_OPTIONS)
        .args(ssh_args)
        .arg(bootstrap_command(&nonce))
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    let mut child = command
        .spawn()
        .map_err(|e| Failure::Connect(format!("failed to spawn ssh: {e}")))?;
    let (Some(stdin), Some(stdout), Some(stderr)) =
        (child.stdin.take(), child.stdout.take(), child.stderr.take())
    else {
        return Err(Failure::Connect("failed to open ssh pipes".into()));
    };
    let stderr_tail = Arc::new(Mutex::new(Vec::new()));
    let tail = Arc::clone(&stderr_tail);
    // Keep draining so a chatty rc file can never block ssh on a full stderr pipe.
    crate::targets::spawn(async move {
        let mut stderr = stderr;
        let mut buffer = [0_u8; 4096];
        while let Ok(read) = stderr.read(&mut buffer).await {
            if read == 0 {
                break;
            }
            let mut tail = tail.lock().unwrap_or_else(|e| e.into_inner());
            tail.extend_from_slice(&buffer[..read]);
            let excess = tail.len().saturating_sub(STDERR_TAIL_BYTES);
            tail.drain(..excess);
        }
    });

    let mut stdout = BufReader::new(stdout);
    let banner = format!("TMUX_MCP_POOL 1 {nonce}");
    let ready = tokio::time::timeout(CONNECT_TIMEOUT, async {
        for _ in 0..MAX_PREAMBLE_LINES {
            let mut line = String::new();
            match stdout.read_line(&mut line).await {
                Ok(0) | Err(_) => return false,
                Ok(_) if line.trim_end() == banner => return true,
                Ok(_) => {}
            }
        }
        false
    })
    .await;
    match ready {
        Ok(true) => Ok(Some(Session {
            child,
            stdin,
            stdout,
            stderr_tail,
            last_used: Instant::now(),
        })),
        Err(_) => Err(Failure::Connect(format!(
            "ssh session connect timed out after {} seconds",
            CONNECT_TIMEOUT.as_secs()
        ))),
        Ok(false) => {
            let status = tokio::time::timeout(Duration::from_secs(5), child.wait()).await;
            let detail = tail_text(&stderr_tail);
            match status {
                // 255 is OpenSSH's own failure (network, auth, host key); nothing ran remotely.
                Ok(Ok(status)) if status.code() == Some(255) => {
                    Err(Failure::Connect(if detail.is_empty() {
                        "ssh: connection failed".to_string()
                    } else {
                        detail
                    }))
                }
                _ => {
                    tracing::warn!(%detail, "ssh session dispatcher did not start");
                    Ok(None)
                }
            }
        }
    }
}

fn tail_text(tail: &Mutex<Vec<u8>>) -> String {
    let tail = tail.lock().unwrap_or_else(|e| e.into_inner());
    String::from_utf8_lossy(&tail).trim().to_string()
}

struct Session {
    child: Child,
    stdin: ChildStdin,
    stdout: BufReader<ChildStdout>,
    stderr_tail: Arc<Mutex<Vec<u8>>>,
    last_used: Instant,
}

impl Session {
    fn alive(&mut self) -> bool {
        matches!(self.child.try_wait(), Ok(None))
    }

    fn lost(&self, what: &str) -> Failure {
        let detail = tail_text(&self.stderr_tail);
        Failure::Lost(if detail.is_empty() {
            format!("ssh session {what}")
        } else {
            format!("ssh session {what}: {detail}")
        })
    }

    async fn ping(&mut self) -> bool {
        let id = NEXT_ID.fetch_add(1, Ordering::Relaxed);
        let expected = format!("P {id}");
        let exchange = async {
            self.stdin
                .write_all(format!("P {id}\n").as_bytes())
                .await
                .ok()?;
            self.stdin.flush().await.ok()?;
            let mut line = String::new();
            (self.stdout.read_line(&mut line).await.ok()? > 0).then_some(line)
        };
        matches!(
            tokio::time::timeout(PING_TIMEOUT, exchange).await,
            Ok(Some(line)) if line.trim_end() == expected
        )
    }

    async fn exchange(
        &mut self,
        line: &str,
        id: u64,
        cap: usize,
        timeout: Duration,
    ) -> Result<RemoteOutput, Failure> {
        // `Err(false)`: the pipe closed; `Err(true)`: an unexpected response line.
        let exchange = async {
            if self.stdin.write_all(line.as_bytes()).await.is_err()
                || self.stdin.flush().await.is_err()
            {
                return Err(false);
            }
            let mut response = String::new();
            match self.stdout.read_line(&mut response).await {
                Ok(0) | Err(_) => Err(false),
                Ok(_) => parse_response(response.trim_end(), id, cap).ok_or(true),
            }
        };
        match tokio::time::timeout(timeout, exchange).await {
            Err(_) => Err(Failure::Timeout),
            Ok(Ok(output)) => Ok(output),
            Ok(Err(false)) => Err(self.lost("closed")),
            Ok(Err(true)) => Err(Failure::Lost("ssh session protocol error".into())),
        }
    }
}

fn parse_response(line: &str, id: u64, cap: usize) -> Option<RemoteOutput> {
    let mut fields = line.split(' ');
    if fields.next()? != "D" || fields.next()?.parse::<u64>().ok()? != id {
        return None;
    }
    let code = fields.next()?.parse::<i32>().ok()?;
    let size = fields.next()?.parse::<u64>().ok()?;
    let decode = |field: &str| match field {
        "-" => Some(Vec::new()),
        data => STANDARD.decode(data).ok(),
    };
    let stdout = decode(fields.next()?)?;
    let stderr = decode(fields.next()?)?;
    if fields.next().is_some() {
        return None;
    }
    Some(RemoteOutput {
        code,
        stdout_truncated: size > cap as u64,
        stdout,
        stderr,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::TmuxStub;

    fn request(
        command: &str,
        stdin: Option<&[u8]>,
        cap: usize,
        timeout_ms: u64,
    ) -> Request<'static> {
        Request {
            command: Box::leak(command.to_string().into_boxed_str()),
            stdin: stdin.map(|input| &*Box::leak(input.to_vec().into_boxed_slice())),
            stdout_cap: cap,
            timeout: Duration::from_millis(timeout_ms),
            idempotent: true,
        }
    }

    async fn done(key: &[String], request: Request<'_>) -> (RemoteOutput, bool) {
        match run(key, request).await {
            Ok(Outcome::Done { output, reused }) => (output, reused),
            other => panic!("expected output, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn session_round_trips_binary_stdin_exit_codes_and_truncation() {
        let mut stub = TmuxStub::new();
        stub.set_var("TMUX_MCP_SSH_POOL", "1");
        let key = vec!["pool-direct".to_string()];

        let input = b"line one\nline two\0binary\xff".to_vec();
        let (output, reused) = done(&key, request("cat", Some(&input), 1024, 10_000)).await;
        assert!(!reused);
        assert_eq!(output.code, 0);
        assert_eq!(output.stdout, input);
        assert!(!output.stdout_truncated);

        let (output, reused) = done(
            &key,
            request("printf 0123456789; echo oops >&2; exit 3", None, 4, 10_000),
        )
        .await;
        assert!(reused, "second request must reuse the session");
        assert_eq!(output.code, 3);
        assert_eq!(output.stdout, b"0123");
        assert!(output.stdout_truncated);
        assert_eq!(output.stderr, b"oops\n");

        // A request reading stdin without input sees EOF, never the request stream.
        let (output, _) = done(&key, request("cat; echo done", None, 1024, 10_000)).await;
        assert_eq!(output.stdout, b"done\n");
    }

    #[tokio::test]
    async fn timed_out_or_lost_session_is_replaced() {
        let mut stub = TmuxStub::new();
        stub.set_var("TMUX_MCP_SSH_POOL", "1");
        let key = vec!["pool-replace".to_string()];
        done(&key, request("true", None, 16, 10_000)).await;

        let slow = run(&key, request("sleep 5", None, 16, 300)).await;
        assert!(matches!(slow, Err(Failure::Timeout)), "got {slow:?}");
        let (output, reused) = done(&key, request("echo fresh", None, 16, 10_000)).await;
        assert!(!reused, "a timed-out session is never reused");
        assert_eq!(output.stdout, b"fresh\n");

        // The dispatcher dies mid-request: report it, never fabricate a result.
        let mut killer = request("kill -9 $PPID", None, 16, 10_000);
        killer.idempotent = false;
        let lost = run(&key, killer).await;
        assert!(
            matches!(&lost, Err(Failure::Lost(message)) if message.contains("ssh session")),
            "got {lost:?}"
        );
        let (output, reused) = done(&key, request("echo again", None, 16, 10_000)).await;
        assert!(!reused);
        assert_eq!(output.stdout, b"again\n");
    }

    /// The dispatcher acts only on a complete request line: a connection cut mid-request
    /// leaves a partial line, which must never run.
    #[tokio::test]
    async fn dispatcher_never_runs_a_partial_request_line() {
        let dir = tempfile::tempdir().unwrap();
        // POSIX sh runs the dispatcher too; on Windows `bash` may resolve to the WSL launcher.
        let mut child = tokio::process::Command::new("sh")
            .arg("-c")
            .arg(DISPATCHER)
            .arg("tmux-mcp-pool")
            .arg("nonce")
            .current_dir(dir.path())
            // Absolute: Git-for-Windows rewrites a bare SHELL=sh relative to the cwd.
            .env("SHELL", "/bin/sh")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .kill_on_drop(true)
            .spawn()
            .expect("start dispatcher");
        let mut stdin = child.stdin.take().unwrap();
        let mut stdout = BufReader::new(child.stdout.take().unwrap());
        // Generous: this checks which requests run, not speed, and MSYS process spawns on a
        // busy Windows test host occasionally take many seconds.
        let step = Duration::from_secs(30);
        let mut banner = String::new();
        tokio::time::timeout(step, stdout.read_line(&mut banner))
            .await
            .expect("banner")
            .unwrap();
        assert_eq!(banner.trim_end(), "TMUX_MCP_POOL 1 nonce");

        let complete = format!("R 1 64 {} -\n", STANDARD.encode("touch complete"));
        let partial = format!("R 2 64 {} -", STANDARD.encode("touch partial"));
        stdin.write_all(complete.as_bytes()).await.unwrap();
        let mut response = String::new();
        tokio::time::timeout(step, stdout.read_line(&mut response))
            .await
            .expect("response")
            .unwrap();
        assert!(response.starts_with("D 1 0 "), "{response}");
        stdin.write_all(partial.as_bytes()).await.unwrap();
        drop(stdin); // connection lost before the newline
        let status = tokio::time::timeout(step, child.wait())
            .await
            .expect("dispatcher exits on EOF")
            .unwrap();
        assert!(status.success());
        assert!(dir.path().join("complete").exists());
        assert!(!dir.path().join("partial").exists(), "partial request ran");
    }

    #[tokio::test]
    async fn host_without_dispatcher_support_is_reported_unsupported() {
        let mut stub = TmuxStub::new();
        stub.set_var("TMUX_MCP_SSH_POOL", "1");
        // mktemp cannot create the private directory, so the dispatcher refuses to start.
        stub.set_var("TMPDIR", "/nonexistent/tmux-mcp-pool-test");
        let key = vec!["pool-unsupported".to_string()];
        assert!(matches!(
            run(&key, request("true", None, 16, 10_000)).await,
            Ok(Outcome::Unsupported)
        ));
        assert!(
            unsupported(&pool_key(&key)),
            "unsupported hosts are remembered"
        );
    }

    #[test]
    fn parse_response_checks_id_and_decodes_fields() {
        let line = format!("D 7 3 5 {} -", STANDARD.encode("hel"));
        let output = parse_response(&line, 7, 3).expect("parsed");
        assert_eq!(output.code, 3);
        assert_eq!(output.stdout, b"hel");
        assert!(output.stdout_truncated);
        assert!(output.stderr.is_empty());
        assert!(parse_response(&line, 8, 3).is_none(), "stale id");
        assert!(parse_response("D 7 0 0 - - extra", 7, 1).is_none());
        assert!(parse_response("X 7 bad-request", 7, 1).is_none());
    }
}
