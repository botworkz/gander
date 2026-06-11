// SPDX-License-Identifier: GPL-3.0-or-later
#![allow(dead_code)]

use std::{
    collections::HashMap,
    io,
    process::{ExitStatus, Stdio},
    sync::{Arc, Mutex},
    time::Duration,
};

use nix::{
    errno::Errno,
    sys::signal::{Signal, kill},
    unistd::Pid,
};
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
    process::{Child, Command},
    sync::{broadcast, mpsc, watch},
    time::timeout,
};
use tokio_stream::wrappers::BroadcastStream;

const EVENT_CHANNEL_CAPACITY: usize = 64;
const INVARIANT_ERROR: &str = "supervisor state corrupted; restart required";

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ChildHandle {
    pub pid: u32,
    pub port: u16,
    pub profile: String,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ChildState {
    Starting,
    Ready,
    Failed(String),
    Exited(i32),
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ChildSnapshot {
    pub handle: Option<ChildHandle>,
    pub state: ChildState,
}

impl ChildSnapshot {
    fn starting() -> Self {
        Self {
            handle: None,
            state: ChildState::Starting,
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct LifecycleEvent {
    pub profile: String,
    pub snapshot: ChildSnapshot,
}

#[derive(Clone)]
pub struct ChildStdio {
    stdin_tx: mpsc::UnboundedSender<String>,
    stdout_tx: broadcast::Sender<String>,
}

impl ChildStdio {
    pub fn send_line(&self, line: impl Into<String>) -> io::Result<()> {
        self.stdin_tx
            .send(line.into())
            .map_err(|_| io::Error::new(io::ErrorKind::BrokenPipe, "child stdin is closed"))
    }

    pub fn subscribe(&self) -> broadcast::Receiver<String> {
        self.stdout_tx.subscribe()
    }
}

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("child for profile `{0}` failed to start: {1}")]
    Failed(String, String),
    #[error("child for profile `{0}` exited with status {1} before becoming ready")]
    Exited(String, i32),
    #[error("supervisor worker for profile `{0}` stopped unexpectedly")]
    Closed(String),
}

pub type Result<T, E = Error> = std::result::Result<T, E>;

pub struct Supervisor<F>
where
    F: Fn(&str) -> io::Result<Command> + Send + Sync + 'static,
{
    inner: Arc<Inner<F>>,
}

struct Inner<F>
where
    F: Fn(&str) -> io::Result<Command> + Send + Sync + 'static,
{
    grace_period: Duration,
    spawn: Arc<F>,
    entries: Mutex<HashMap<String, ChildEntry>>,
    events: broadcast::Sender<LifecycleEvent>,
}

struct ChildEntry {
    control_tx: Option<mpsc::UnboundedSender<Control>>,
    snapshot_rx: watch::Receiver<ChildSnapshot>,
    stdio_rx: watch::Receiver<Option<ChildStdio>>,
}

enum Control {
    Stop,
}

impl<F> Supervisor<F>
where
    F: Fn(&str) -> io::Result<Command> + Send + Sync + 'static,
{
    pub fn new(spawn: F, grace_period: Duration) -> Self {
        let (events, _) = broadcast::channel(EVENT_CHANNEL_CAPACITY);
        Self {
            inner: Arc::new(Inner {
                grace_period,
                spawn: Arc::new(spawn),
                entries: Mutex::new(HashMap::new()),
                events,
            }),
        }
    }

    pub async fn ensure_running(&self, profile: &str) -> Result<ChildHandle> {
        let mut snapshot_rx = {
            let mut entries = self.inner.entries.lock().expect(INVARIANT_ERROR);
            if let Some(entry) = entries.get(profile) {
                let snapshot = entry.snapshot_rx.borrow().clone();
                match snapshot.state {
                    ChildState::Ready => {
                        return Ok(ready_handle(snapshot));
                    }
                    ChildState::Starting => entry.snapshot_rx.clone(),
                    ChildState::Failed(_) | ChildState::Exited(_) => {
                        let new_entry = spawn_child(
                            profile,
                            Arc::clone(&self.inner.spawn),
                            self.inner.grace_period,
                            self.inner.events.clone(),
                        );
                        let snapshot_rx = new_entry.snapshot_rx.clone();
                        entries.insert(profile.to_owned(), new_entry);
                        snapshot_rx
                    }
                }
            } else {
                let entry = spawn_child(
                    profile,
                    Arc::clone(&self.inner.spawn),
                    self.inner.grace_period,
                    self.inner.events.clone(),
                );
                let snapshot_rx = entry.snapshot_rx.clone();
                entries.insert(profile.to_owned(), entry);
                snapshot_rx
            }
        };

        loop {
            let snapshot = snapshot_rx.borrow().clone();
            match snapshot.state {
                ChildState::Ready => {
                    return Ok(ready_handle(snapshot));
                }
                ChildState::Failed(message) => {
                    return Err(Error::Failed(profile.to_owned(), message));
                }
                ChildState::Exited(code) => {
                    return Err(Error::Exited(profile.to_owned(), code));
                }
                ChildState::Starting => {}
            }
            snapshot_rx
                .changed()
                .await
                .map_err(|_| Error::Closed(profile.to_owned()))?;
        }
    }

    pub async fn stop(&self, profile: &str) -> Result<()> {
        let mut snapshot_rx = {
            let entries = self.inner.entries.lock().expect(INVARIANT_ERROR);
            let Some(entry) = entries.get(profile) else {
                return Ok(());
            };
            if let Some(control_tx) = &entry.control_tx {
                let _ = control_tx.send(Control::Stop);
            }
            entry.snapshot_rx.clone()
        };

        while matches!(
            snapshot_rx.borrow().state,
            ChildState::Starting | ChildState::Ready
        ) {
            snapshot_rx
                .changed()
                .await
                .map_err(|_| Error::Closed(profile.to_owned()))?;
        }

        Ok(())
    }

    pub async fn restart(&self, profile: &str) -> Result<ChildHandle> {
        self.stop(profile).await?;
        self.ensure_running(profile).await
    }

    pub fn snapshot(&self, profile: &str) -> Option<ChildSnapshot> {
        self.inner
            .entries
            .lock()
            .expect(INVARIANT_ERROR)
            .get(profile)
            .map(|entry| entry.snapshot_rx.borrow().clone())
    }

    pub fn subscribe(&self) -> BroadcastStream<LifecycleEvent> {
        BroadcastStream::new(self.inner.events.subscribe())
    }

    pub fn stdio(&self, profile: &str) -> Option<ChildStdio> {
        self.inner
            .entries
            .lock()
            .expect(INVARIANT_ERROR)
            .get(profile)
            .and_then(|entry| entry.stdio_rx.borrow().clone())
    }
}

impl<F> Clone for Supervisor<F>
where
    F: Fn(&str) -> io::Result<Command> + Send + Sync + 'static,
{
    fn clone(&self) -> Self {
        Self {
            inner: Arc::clone(&self.inner),
        }
    }
}

impl<F> Drop for Inner<F>
where
    F: Fn(&str) -> io::Result<Command> + Send + Sync + 'static,
{
    fn drop(&mut self) {
        for entry in self.entries.lock().expect(INVARIANT_ERROR).values() {
            if let Some(control_tx) = &entry.control_tx {
                let _ = control_tx.send(Control::Stop);
            }
        }
    }
}

fn spawn_child<F>(
    profile: &str,
    spawn: Arc<F>,
    grace_period: Duration,
    events: broadcast::Sender<LifecycleEvent>,
) -> ChildEntry
where
    F: Fn(&str) -> io::Result<Command> + Send + Sync + 'static,
{
    let profile = profile.to_owned();
    let (snapshot_tx, snapshot_rx) = watch::channel(ChildSnapshot::starting());
    let (stdio_tx, stdio_rx) = watch::channel(None);
    let (control_tx, control_rx) = mpsc::unbounded_channel();
    publish(&events, &profile, &ChildSnapshot::starting());

    tokio::spawn(run_child(
        profile,
        spawn,
        grace_period,
        events,
        snapshot_tx,
        stdio_tx,
        control_rx,
    ));

    ChildEntry {
        control_tx: Some(control_tx),
        snapshot_rx,
        stdio_rx,
    }
}

async fn run_child<F>(
    profile: String,
    spawn: Arc<F>,
    grace_period: Duration,
    events: broadcast::Sender<LifecycleEvent>,
    snapshot_tx: watch::Sender<ChildSnapshot>,
    stdio_tx: watch::Sender<Option<ChildStdio>>,
    mut control_rx: mpsc::UnboundedReceiver<Control>,
) where
    F: Fn(&str) -> io::Result<Command> + Send + Sync + 'static,
{
    let mut command = match spawn(&profile) {
        Ok(command) => command,
        Err(error) => {
            update_snapshot(
                &snapshot_tx,
                &events,
                &profile,
                ChildSnapshot {
                    handle: None,
                    state: ChildState::Failed(error.to_string()),
                },
            );
            return;
        }
    };
    command.stdin(Stdio::piped());
    command.stdout(Stdio::piped());

    let mut child = match command.spawn() {
        Ok(child) => child,
        Err(error) => {
            update_snapshot(
                &snapshot_tx,
                &events,
                &profile,
                ChildSnapshot {
                    handle: None,
                    state: ChildState::Failed(error.to_string()),
                },
            );
            return;
        }
    };

    let pid = match child.id() {
        Some(pid) => pid,
        None => {
            let _ = child.start_kill();
            let _ = child.wait().await;
            update_snapshot(
                &snapshot_tx,
                &events,
                &profile,
                ChildSnapshot {
                    handle: None,
                    state: ChildState::Failed("child pid unavailable after spawn".to_owned()),
                },
            );
            return;
        }
    };
    let stdout = match child.stdout.take() {
        Some(stdout) => stdout,
        None => {
            update_snapshot(
                &snapshot_tx,
                &events,
                &profile,
                ChildSnapshot {
                    handle: None,
                    state: ChildState::Failed("child stdout was not piped".to_owned()),
                },
            );
            return;
        }
    };
    let stdin = match child.stdin.take() {
        Some(stdin) => stdin,
        None => {
            update_snapshot(
                &snapshot_tx,
                &events,
                &profile,
                ChildSnapshot {
                    handle: None,
                    state: ChildState::Failed("child stdin was not piped".to_owned()),
                },
            );
            return;
        }
    };
    let (stdin_tx, mut stdin_rx) = mpsc::unbounded_channel::<String>();
    tokio::spawn(async move {
        let mut stdin = stdin;
        while let Some(line) = stdin_rx.recv().await {
            if stdin.write_all(line.as_bytes()).await.is_err() {
                break;
            }
            if stdin.write_all(b"\n").await.is_err() {
                break;
            }
            if stdin.flush().await.is_err() {
                break;
            }
        }
    });
    let (stdout_tx, _) = broadcast::channel(EVENT_CHANNEL_CAPACITY);
    let mut lines = BufReader::new(stdout).lines();

    let port_line = tokio::select! {
        control = control_rx.recv() => {
            if matches!(control, Some(Control::Stop)) {
                finish_stop(&mut child, grace_period, &snapshot_tx, &events, &profile).await;
                return;
            }
            return;
        }
        line = lines.next_line() => line,
        status = child.wait() => {
            let state = match status {
                Ok(status) => ChildState::Failed(format!(
                    "process exited before reporting a port ({})",
                    describe_status(status)
                )),
                Err(error) => ChildState::Failed(format!("failed waiting for child: {error}")),
            };
            update_snapshot(&snapshot_tx, &events, &profile, ChildSnapshot { handle: None, state });
            return;
        }
    };

    let port_line = match port_line {
        Ok(Some(port_line)) => port_line,
        Ok(None) => {
            update_snapshot(
                &snapshot_tx,
                &events,
                &profile,
                ChildSnapshot {
                    handle: None,
                    state: ChildState::Failed(
                        "child closed stdout before reporting a port".to_owned(),
                    ),
                },
            );
            return;
        }
        Err(error) => {
            update_snapshot(
                &snapshot_tx,
                &events,
                &profile,
                ChildSnapshot {
                    handle: None,
                    state: ChildState::Failed(format!("failed reading child stdout: {error}")),
                },
            );
            return;
        }
    };

    let port = match port_line.trim().parse::<u16>() {
        Ok(port) => port,
        Err(error) => {
            update_snapshot(
                &snapshot_tx,
                &events,
                &profile,
                ChildSnapshot {
                    handle: None,
                    state: ChildState::Failed(format!("invalid port `{port_line}`: {error}")),
                },
            );
            return;
        }
    };

    update_snapshot(
        &snapshot_tx,
        &events,
        &profile,
        ChildSnapshot {
            handle: Some(ChildHandle {
                pid,
                port,
                profile: profile.clone(),
            }),
            state: ChildState::Ready,
        },
    );
    let _ = stdio_tx.send(Some(ChildStdio {
        stdin_tx,
        stdout_tx: stdout_tx.clone(),
    }));
    tokio::spawn(async move {
        loop {
            match lines.next_line().await {
                Ok(Some(line)) => {
                    let _ = stdout_tx.send(line);
                }
                Ok(None) | Err(_) => break,
            }
        }
    });

    tokio::select! {
        control = control_rx.recv() => {
            if matches!(control, Some(Control::Stop)) {
                finish_stop(&mut child, grace_period, &snapshot_tx, &events, &profile).await;
            }
        }
        status = child.wait() => {
            let state = match status {
                Ok(status) => ChildState::Failed(format!(
                    "process exited unexpectedly ({})",
                    describe_status(status)
                )),
                Err(error) => ChildState::Failed(format!("failed waiting for child: {error}")),
            };
            update_snapshot(
                &snapshot_tx,
                &events,
                &profile,
                ChildSnapshot {
                    handle: None,
                    state,
                },
            );
        }
    }
    let _ = stdio_tx.send(None);
}

async fn finish_stop(
    child: &mut Child,
    grace_period: Duration,
    snapshot_tx: &watch::Sender<ChildSnapshot>,
    events: &broadcast::Sender<LifecycleEvent>,
    profile: &str,
) {
    let state = match stop_child(child, grace_period).await {
        Ok(status) => ChildState::Exited(exit_code(status)),
        Err(error) => ChildState::Failed(format!("failed stopping child: {error}")),
    };
    update_snapshot(
        snapshot_tx,
        events,
        profile,
        ChildSnapshot {
            handle: None,
            state,
        },
    );
}

async fn stop_child(child: &mut Child, grace_period: Duration) -> io::Result<ExitStatus> {
    if let Some(pid) = child.id() {
        send_signal(pid, Some(Signal::SIGTERM))?;
    }

    match timeout(grace_period, child.wait()).await {
        Ok(status) => status,
        Err(_) => {
            if let Some(pid) = child.id() {
                let _ = send_signal(pid, Some(Signal::SIGKILL));
            }
            child.wait().await
        }
    }
}

fn update_snapshot(
    snapshot_tx: &watch::Sender<ChildSnapshot>,
    events: &broadcast::Sender<LifecycleEvent>,
    profile: &str,
    snapshot: ChildSnapshot,
) {
    let _ = snapshot_tx.send(snapshot.clone());
    publish(events, profile, &snapshot);
}

fn ready_handle(snapshot: ChildSnapshot) -> ChildHandle {
    snapshot.handle.expect(INVARIANT_ERROR)
}

fn publish(events: &broadcast::Sender<LifecycleEvent>, profile: &str, snapshot: &ChildSnapshot) {
    let _ = events.send(LifecycleEvent {
        profile: profile.to_owned(),
        snapshot: snapshot.clone(),
    });
}

fn send_signal(pid: u32, signal: Option<Signal>) -> io::Result<()> {
    match kill(Pid::from_raw(pid as i32), signal) {
        Ok(()) | Err(Errno::ESRCH) => Ok(()),
        Err(errno) => Err(io::Error::from_raw_os_error(errno as i32)),
    }
}

fn describe_status(status: ExitStatus) -> String {
    #[cfg(unix)]
    {
        use std::os::unix::process::ExitStatusExt;
        if let Some(code) = status.code() {
            return format!("exit code {code}");
        }
        if let Some(signal) = status.signal() {
            return format!("signal {signal}");
        }
    }

    status
        .code()
        .map(|code| format!("exit code {code}"))
        .unwrap_or_else(|| "unknown status".to_owned())
}

fn exit_code(status: ExitStatus) -> i32 {
    #[cfg(unix)]
    {
        use std::os::unix::process::ExitStatusExt;
        if let Some(code) = status.code() {
            return code;
        }
        if let Some(signal) = status.signal() {
            return -signal;
        }
    }

    status.code().unwrap_or(-1)
}

#[cfg(test)]
mod tests {
    use std::{
        net::TcpListener,
        sync::{
            Arc,
            atomic::{AtomicUsize, Ordering},
        },
        time::Instant,
    };

    use super::*;
    use tokio_stream::StreamExt;

    fn reserve_port() -> u16 {
        TcpListener::bind("127.0.0.1:0")
            .unwrap()
            .local_addr()
            .unwrap()
            .port()
    }

    fn shell_command(script: &'static str) -> impl Fn(&str) -> io::Result<Command> + Send + Sync {
        move |profile| {
            let mut command = Command::new("sh");
            command.arg("-c").arg(script);
            command.env("PROFILE_NAME", profile);
            command.env("SUPERVISOR_PORT", reserve_port().to_string());
            Ok(command)
        }
    }

    async fn wait_for_state<FN>(
        supervisor: &Supervisor<FN>,
        profile: &str,
        predicate: impl Fn(&ChildState) -> bool,
    ) -> ChildSnapshot
    where
        FN: Fn(&str) -> io::Result<Command> + Send + Sync + 'static,
    {
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            let snapshot = supervisor.snapshot(profile).unwrap();
            if predicate(&snapshot.state) {
                return snapshot;
            }
            assert!(Instant::now() < deadline, "timed out waiting for state");
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    }

    fn process_exists(pid: u32) -> bool {
        matches!(
            kill(Pid::from_raw(pid as i32), None),
            Ok(()) | Err(Errno::EPERM)
        )
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn spawn_and_ready_happy_path() {
        let supervisor = Supervisor::new(
            shell_command(r#"printf '%s\n' "$SUPERVISOR_PORT"; exec sleep 30"#),
            Duration::from_millis(100),
        );
        let mut events = supervisor.subscribe();

        let handle = supervisor.ensure_running("alpha").await.unwrap();
        let same = supervisor.ensure_running("alpha").await.unwrap();

        assert_eq!(handle, same);
        assert!(handle.port > 0);
        assert_eq!(
            supervisor.snapshot("alpha").unwrap().state,
            ChildState::Ready
        );

        let starting = events.next().await.unwrap().unwrap();
        let ready = events.next().await.unwrap().unwrap();
        assert_eq!(starting.profile, "alpha");
        assert_eq!(starting.snapshot.state, ChildState::Starting);
        assert_eq!(ready.profile, "alpha");
        assert_eq!(ready.snapshot.state, ChildState::Ready);

        supervisor.stop("alpha").await.unwrap();
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn graceful_stop_within_grace_period() {
        let supervisor = Supervisor::new(
            shell_command(r#"printf '%s\n' "$SUPERVISOR_PORT"; exec sleep 30"#),
            Duration::from_secs(1),
        );

        let handle = supervisor.ensure_running("alpha").await.unwrap();
        supervisor.stop("alpha").await.unwrap();

        let snapshot = supervisor.snapshot("alpha").unwrap();
        assert!(matches!(snapshot.state, ChildState::Exited(_)));
        assert!(!process_exists(handle.pid));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn forced_kill_when_grace_period_expires() {
        let supervisor = Supervisor::new(
            shell_command(
                r#"trap '' TERM; printf '%s\n' "$SUPERVISOR_PORT"; while :; do sleep 1; done"#,
            ),
            Duration::from_millis(100),
        );

        let handle = supervisor.ensure_running("alpha").await.unwrap();
        supervisor.stop("alpha").await.unwrap();

        let snapshot = supervisor.snapshot("alpha").unwrap();
        assert!(matches!(snapshot.state, ChildState::Exited(code) if code != 0));
        assert!(!process_exists(handle.pid));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn spontaneous_exit_transitions_to_failed() {
        let supervisor = Supervisor::new(
            shell_command(r#"printf '%s\n' "$SUPERVISOR_PORT"; sleep 0.2; exit 17"#),
            Duration::from_millis(100),
        );

        let handle = supervisor.ensure_running("alpha").await.unwrap();
        assert!(process_exists(handle.pid));

        let snapshot = wait_for_state(&supervisor, "alpha", |state| {
            matches!(state, ChildState::Failed(_))
        })
        .await;
        assert!(
            matches!(snapshot.state, ChildState::Failed(message) if message.contains("unexpectedly"))
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn restart_resets_a_failed_child() {
        let starts = Arc::new(AtomicUsize::new(0));
        let supervisor = Supervisor::new(
            {
                let starts = Arc::clone(&starts);
                move |profile| {
                    let mut command = Command::new("sh");
                    command.env("PROFILE_NAME", profile);
                    command.env("SUPERVISOR_PORT", reserve_port().to_string());
                    if starts.fetch_add(1, Ordering::SeqCst) == 0 {
                        command
                            .arg("-c")
                            .arg(r#"printf '%s\n' "$SUPERVISOR_PORT"; sleep 0.2; exit 17"#);
                    } else {
                        command
                            .arg("-c")
                            .arg(r#"printf '%s\n' "$SUPERVISOR_PORT"; exec sleep 30"#);
                    }
                    Ok(command)
                }
            },
            Duration::from_millis(100),
        );

        let first = supervisor.ensure_running("alpha").await.unwrap();
        wait_for_state(&supervisor, "alpha", |state| {
            matches!(state, ChildState::Failed(_))
        })
        .await;

        let restarted = supervisor.restart("alpha").await.unwrap();
        assert_ne!(first.pid, restarted.pid);
        assert_eq!(
            supervisor.snapshot("alpha").unwrap().state,
            ChildState::Ready
        );

        supervisor.stop("alpha").await.unwrap();
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn dropping_supervisor_stops_all_children() {
        let pid = {
            let supervisor = Supervisor::new(
                shell_command(r#"printf '%s\n' "$SUPERVISOR_PORT"; exec sleep 30"#),
                Duration::from_millis(100),
            );
            supervisor.ensure_running("alpha").await.unwrap().pid
        };

        let deadline = Instant::now() + Duration::from_secs(5);
        while process_exists(pid) {
            assert!(
                Instant::now() < deadline,
                "child was not cleaned up on drop"
            );
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn profiles_have_independent_lifecycles() {
        let supervisor = Supervisor::new(
            shell_command(r#"printf '%s\n' "$SUPERVISOR_PORT"; exec sleep 30"#),
            Duration::from_millis(100),
        );

        let alpha = supervisor.ensure_running("alpha").await.unwrap();
        let beta = supervisor.ensure_running("beta").await.unwrap();

        supervisor.stop("alpha").await.unwrap();

        assert!(matches!(
            supervisor.snapshot("alpha").unwrap().state,
            ChildState::Exited(_)
        ));
        assert_eq!(
            supervisor.snapshot("beta").unwrap().state,
            ChildState::Ready
        );
        assert!(process_exists(beta.pid));
        assert!(!process_exists(alpha.pid));

        supervisor.stop("beta").await.unwrap();
    }
}
