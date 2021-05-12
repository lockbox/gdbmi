use std::{
    borrow::Cow, collections::HashMap, fmt, num::NonZeroUsize, process::Stdio, time::Duration,
};

use breakpoint::{Breakpoint, LineSpec};
use camino::{Utf8Path, Utf8PathBuf};
use checkpoint::Checkpoint;
use rand::Rng;
use status::Status;
use tokio::{io, process, sync::mpsc, time};
use tracing::{debug, error};
use variable::Variable;

pub mod breakpoint;
pub mod checkpoint;
pub mod parser;
pub mod raw;
pub mod status;
mod string_stream;
pub mod symbol;
pub mod variable;
mod worker;

#[cfg(test)]
mod test_common;

#[derive(Debug, Clone, thiserror::Error, Eq, PartialEq)]
pub enum Error {
    #[error(transparent)]
    Gdb(#[from] GdbError),

    #[error("Expected result response")]
    ExpectedResultResponse,

    #[error("Expected a different payload from gdb")]
    /// Parsed, but inconsistent with what sort of payload we expected
    ExpectedDifferentPayload,

    #[error("Expected response to have a payload")]
    ExpectedPayload,

    #[error("Failed to parse payload value as u32")]
    ParseU32(#[from] std::num::ParseIntError),

    #[error("Failed to parse payload value as hex")]
    ParseHex(#[from] ParseHexError),

    #[error("Expected response to have message {expected}, got {actual}")]
    UnexpectedResponseMessage { expected: String, actual: String },

    #[error("Expected different console output in reply to command")]
    ExpectedDifferentConsole,

    #[error(transparent)]
    Timeout(#[from] TimeoutError),
}

// TODO: Remove inner, move code into Gdb

#[derive(Debug, Clone, thiserror::Error, Eq, PartialEq)]
/// Timed out waiting for a message
///
/// This indicates that either gdb or the actor responsible for communicating
/// with it is busy.
///
/// The actor divides its time fairly between reading messages from gdb and
/// handling requests you send to it. It may be overwhelmed if the program being
/// debugger sends too much to stdout or stderr.
#[error("Timed out waiting for a message")]
pub struct TimeoutError;

#[derive(Debug, Clone, thiserror::Error, displaydoc::Display, Eq, PartialEq)]
/// Received error from gdb. Code: {code:?}, msg: {msg:?}
pub struct GdbError {
    code: Option<String>,
    msg: Option<String>,
}

#[derive(Debug, Clone, thiserror::Error, Eq, PartialEq)]
pub enum ParseHexError {
    #[error("Expected to start with 0x")]
    InvalidPrefix,
    #[error(transparent)]
    ParseInt(#[from] std::num::ParseIntError),
}

pub struct Gdb {
    worker: mpsc::UnboundedSender<worker::Msg>,
    timeout: Duration,
}

/// # Warning:
///
/// **Never pass untrusted user input.**
///
/// GDB is designed around the assumption the user is running it on their own
/// machine, and therefore doesn't need to be defended against.
///
/// We do some escaping before passing inputs to GDB to try and protect against
/// users mistakenly entering nonsensical inputs (like `"--type"` as a variable
/// name), but defending against untrusted users is out-of-scope. Use a sandbox.
impl Gdb {
    /// Spawn a gdb process to communicate with.
    ///
    /// The timeout applies to all requests sent to gdb.
    ///
    /// We provide the arguments "--interpreter=mi3" and "--quiet" to the command.
    ///
    /// If you are connecting to the gdbserver in [rr][rr] start it with the
    /// argument `--mark-stdio` so we can distinguish the process output.
    pub fn spawn(executable: impl AsRef<Utf8Path>, timeout: Duration) -> io::Result<Self> {
        let executable = executable.as_ref().as_str();
        debug!(?timeout, "Spawning {}", executable);

        let cmd = process::Command::new("gdb")
            .arg("--interpreter=mi3")
            .arg("--quiet")
            .arg(executable)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()?;

        Ok(Self::new(cmd, timeout))
    }

    /// Like spawn, but spawns `rust-gdb` instead of `gdb`.
    pub fn spawn_rust(executable: impl AsRef<Utf8Path>, timeout: Duration) -> io::Result<Self> {
        let executable = executable.as_ref().as_str();
        debug!(?timeout, "Spawning {}", executable);

        let cmd = process::Command::new("rust-gdb")
            .arg("--interpreter=mi3")
            .arg("--quiet")
            .arg(executable)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()?;

        Ok(Self::new(cmd, timeout))
    }

    /// Spawn the process yourself.
    ///
    /// You are responsible for configuring the process to speak version 3 of
    /// GDB/MI, and setting stdin, stdout, and stderr to [`Stdio::piped`]. The
    /// following is roughly what [`Self::spawn`] does for you.
    ///
    /// ```rust
    /// # use gdbmi::Gdb;
    /// # use std::{process::Stdio, time::Duration};
    /// # tokio_test::block_on(async {
    /// #
    /// let executable = "program-to-debug";
    /// let timeout = Duration::from_secs(5);
    ///
    /// let cmd = tokio::process::Command::new("gdb")
    ///    .arg("--interpreter=mi3")
    ///    .arg("--quiet")
    ///    .arg(executable)
    ///    .stdin(Stdio::piped())
    ///    .stdout(Stdio::piped())
    ///    .stderr(Stdio::piped())
    ///    .spawn()?;
    ///
    /// let gdb = Gdb::new(cmd, timeout);
    /// #
    /// # Ok::<_, Box<dyn std::error::Error>>(())
    /// # });
    /// ```
    ///
    /// See [`Self::spawn`] for an explanation of `timeout`.
    pub fn new(cmd: process::Child, timeout: Duration) -> Self {
        let worker = worker::spawn(cmd);
        Self { worker, timeout }
    }

    /// Note: The status is refreshed when gdb sends us notifications. Calling
    /// this function just fetches the cached status.
    pub async fn status(&self) -> Result<Status, TimeoutError> {
        let (out_tx, out_rx) = mpsc::channel(1);
        self.worker_send(worker::Msg::Status(out_tx));
        Self::worker_receive(out_rx, self.timeout).await
    }

    /// Wait for the status to change and return the new status.
    ///
    /// To avoid missing a status change right before your request is processed,
    /// submit what you think the current status is. If you're wrong, you'll get
    /// back the current status instead of waiting for the next one.
    ///
    /// If you don't specify a timeout the default timeout for this instance
    /// will be used.
    pub async fn next_status(
        &self,
        current: Status,
        timeout: Option<Duration>,
    ) -> Result<Status, TimeoutError> {
        let timeout = timeout.unwrap_or(self.timeout);
        let (out_tx, out_rx) = mpsc::channel(1);
        self.worker_send(worker::Msg::NextStatus {
            current,
            out: out_tx,
        });
        Self::worker_receive(out_rx, timeout).await
    }

    pub async fn await_stopped(
        &self,
        timeout: Option<Duration>,
    ) -> Result<status::Stopped, TimeoutError> {
        let status = self
            .await_status(|s| matches!(s, Status::Stopped(_)), timeout)
            .await?;
        match status {
            Status::Stopped(status) => Ok(status),
            _ => unreachable!(),
        }
    }

    pub async fn await_status<P>(
        &self,
        pred: P,
        timeout: Option<Duration>,
    ) -> Result<Status, TimeoutError>
    where
        P: Fn(&Status) -> bool + Send + Sync + 'static,
    {
        let timeout = timeout.unwrap_or(self.timeout);
        let (out_tx, out_rx) = mpsc::channel(1);
        self.worker_send(worker::Msg::AwaitStatus {
            pred: Box::new(pred),
            out: out_tx,
        });
        Self::worker_receive(out_rx, timeout).await
    }

    pub async fn exec_run(&self) -> Result<(), Error> {
        self.raw_cmd("-exec-run")
            .await?
            .expect_result()?
            .expect_msg_is("running")
    }

    pub async fn exec_continue(&self) -> Result<(), Error> {
        self.raw_cmd("-exec-continue")
            .await?
            .expect_result()?
            .expect_msg_is("running")
    }

    pub async fn exec_continue_reverse(&self) -> Result<(), Error> {
        self.raw_cmd("-exec-continue --reverse")
            .await?
            .expect_result()?
            .expect_msg_is("running")
    }

    pub async fn break_insert(&self, at: LineSpec) -> Result<Breakpoint, Error> {
        let raw = self
            .raw_cmd(format!("-break-insert {}", at.serialize()))
            .await?
            .expect_result()?
            .expect_payload()?
            .remove_expect("bkpt")?
            .expect_dict()?;

        Breakpoint::from_raw(raw)
    }

    pub async fn break_disable<'a, I>(&self, breakpoints: I) -> Result<(), Error>
    where
        I: IntoIterator<Item = &'a Breakpoint>,
    {
        let mut raw = String::new();
        for bp in breakpoints {
            raw.push_str(&format!("{} ", bp.number));
        }

        self.raw_cmd(format!("-break-disable {}", raw))
            .await?
            .expect_result()?
            .expect_msg_is("done")
    }

    pub async fn break_delete<'a, I>(&self, breakpoints: I) -> Result<(), Error>
    where
        I: IntoIterator<Item = &'a Breakpoint>,
    {
        let mut raw = String::new();
        for bp in breakpoints {
            raw.push_str(&format!("{} ", bp.number));
        }

        self.raw_cmd(format!("-break-delete {}", raw))
            .await?
            .expect_result()?
            .expect_msg_is("done")
    }

    /// If `max` is provided, don't count beyond it.
    pub async fn stack_depth(&self, max: Option<u32>) -> Result<u32, Error> {
        let msg = if let Some(max) = max {
            Cow::Owned(format!("-stack-info-depth {}", max))
        } else {
            Cow::Borrowed("-stack-info-depth")
        };
        self.raw_cmd(msg)
            .await?
            .expect_result()?
            .expect_payload()?
            .remove_expect("depth")?
            .expect_number()
    }

    /// List the arguments and local variables in the current stack frame.
    ///
    /// Complex variables (structs, arrays, and unions) will not have a type.
    ///
    /// If `frame_filters` is false python frame filters will be skipped
    pub async fn stack_list_variables(&self, frame_filters: bool) -> Result<Vec<Variable>, Error> {
        let msg = if frame_filters {
            "-stack-list-variables --simple-values"
        } else {
            "-stack-list-variables --no-frame-filters --simple-values"
        };
        let payload = self.raw_cmd(msg).await?.expect_result()?.expect_payload()?;
        variable::from_stack_list(payload)
    }

    pub async fn symbol_info_functions(
        &self,
    ) -> Result<HashMap<Utf8PathBuf, Vec<symbol::Function>>, Error> {
        let payload = self
            .raw_cmd("-symbol-info-functions")
            .await?
            .expect_result()?
            .expect_payload()?;
        symbol::from_symbol_info_functions_payload(payload)
    }

    /// Save a snapshot of the current program state to come back to later.
    ///
    /// If this isn't supported you may get an unhelpful error such as
    ///
    /// ```plain
    /// syntax error in expression, near `lseek (0, 0, 1)'.
    /// ```
    ///
    /// I use this with the time travelling debugger rr, as gdb on my machine
    /// doesn't support snapshots.
    pub async fn save_checkpoint(&self) -> Result<Checkpoint, Error> {
        let (resp, lines) = self.raw_console_cmd_for_output("checkpoint", 1).await?;
        resp.expect_result()?.expect_msg_is("done")?;
        checkpoint::parse_save_line(&lines[0])
    }

    pub async fn goto_checkpoint(&self, checkpoint: Checkpoint) -> Result<(), Error> {
        self.raw_console_cmd(format!("restart {}", checkpoint.0))
            .await?
            .expect_result()?
            .expect_msg_is("running")
    }

    /// Execute a command for a response.
    ///
    /// Your command will be prefixed with a token and suffixed with a newline.
    pub async fn raw_cmd(&self, msg: impl Into<String>) -> Result<raw::Response, Error> {
        let token = Token::generate();
        let (out_tx, out_rx) = mpsc::channel(1);
        self.worker_send(worker::Msg::Cmd {
            token,
            msg: msg.into(),
            out: out_tx,
        });
        Self::worker_receive(out_rx, self.timeout).await?
    }

    /// Execute a console command for a given number of lines of console output.
    ///
    /// Console commands are the commands you enter in a normal GDB session,
    /// in contrast to the GDB/MI commands designed for programmatic use.
    ///
    /// You will need to use this function if the command you need isn't
    /// supported by GDB/MI.
    ///
    /// If you need to see the output, use
    /// [`Self::execute_raw_console_for_output`].
    pub async fn raw_console_cmd(&self, msg: impl Into<String>) -> Result<raw::Response, Error> {
        let msg = msg.into();
        assert!(
            !msg.contains('"'),
            "Cannot execute raw console command containing double quote character"
        );
        let msg = format!(r#"-interpreter-exec console "{}""#, escape_arg(msg));

        self.raw_cmd(msg).await
    }

    /// Prefer [`Self::execute_raw_console`] if possible.
    ///
    /// Avoid capturing more lines than you need to. Because console output
    /// can't be associated with a command we assume the next `capture_lines` of
    /// output should go to the caller. This means we  need to block anyone else
    /// from communicating with to GDB until the lines are received or you timeout.
    ///
    /// # Panics
    /// - `capture_lines` is zero
    pub async fn raw_console_cmd_for_output(
        &self,
        msg: impl AsRef<str>,
        capture_lines: usize,
    ) -> Result<(raw::Response, Vec<String>), Error> {
        let msg = format!(r#"-interpreter-exec console "{}""#, escape_arg(msg));
        let capture_lines = NonZeroUsize::new(capture_lines).expect("capture_lines nonzero");

        // Ensure no output is going to come for earlier commands
        self.await_ready().await?;

        let token = Token::generate();
        let (out_tx, out_rx) = mpsc::channel(1);

        self.worker_send(worker::Msg::ConsoleCmd {
            token,
            msg,
            out: out_tx,
            capture_lines,
        });

        Self::worker_receive(out_rx, self.timeout).await?
    }

    /// Waits until gdb is responsive to commands.
    ///
    /// You do not need to call this before sending commands yourself.
    pub async fn await_ready(&self) -> Result<(), Error> {
        // Arbitrary command, chosen because its output isn't too big
        self.raw_cmd("-list-target-features").await?;
        Ok(())
    }

    /// Pop any messages gdb has sent that weren't addressed to any specific
    /// request off the buffer and return them.
    pub async fn pop_general(&self) -> Result<Vec<raw::GeneralMessage>, TimeoutError> {
        let (out_tx, out_rx) = mpsc::channel(1);
        self.worker_send(worker::Msg::PopGeneral(out_tx));
        Self::worker_receive(out_rx, self.timeout).await
    }

    /// Change the timeout used for all async operations
    pub fn set_timeout(&mut self, timeout: Duration) {
        self.timeout = timeout;
    }

    fn worker_send(&self, msg: worker::Msg) {
        self.worker.send(msg).expect("Can send to mainloop");
    }

    async fn worker_receive<O: std::fmt::Debug>(
        mut rx: mpsc::Receiver<O>,
        timeout: Duration,
    ) -> Result<O, TimeoutError> {
        time::timeout(timeout, rx.recv())
            .await
            .map(|o| o.expect("out chan not closed"))
            .map_err(|_| TimeoutError)
    }
}

impl fmt::Debug for Gdb {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Gdb").finish() // TODO: Use finish_non_exhaustive
    }
}

/// Warning: This is on a best-effort basis, based on what someone on the gdb
/// irc thought made sense.
fn escape_arg(arg: impl AsRef<str>) -> String {
    let arg = arg.as_ref();
    let mut out = String::with_capacity(arg.len() + 2);
    out.push('"');
    for c in arg.chars() {
        if c == '"' {
            out.push('\\');
            out.push('"');
        } else {
            out.push(c);
        }
    }
    out.push('"');
    out
}

#[derive(Debug, Copy, Clone, Eq, PartialEq, Hash)]
struct Token(u32);

impl Token {
    fn generate() -> Self {
        Self(rand::thread_rng().gen())
    }

    pub(crate) fn serialize(&self) -> String {
        format!("{}", self.0)
    }
}

#[cfg(test)]
mod tests {
    use std::{collections::BTreeMap, iter};

    use crate::{
        status::{ExitReason, StopReason},
        test_common::record_hello_world,
    };

    use super::*;
    use insta::assert_debug_snapshot;
    use pretty_assertions::assert_eq;
    use test_common::{build_hello_world, init, Result, TIMEOUT};

    // TODO: Replace assert!(matches!) with assert_matches! when stable

    fn fixture() -> eyre::Result<Gdb> {
        init();
        let bin = build_hello_world();
        Ok(Gdb::spawn_rust(bin, TIMEOUT)?)
    }

    fn rr_fixture() -> eyre::Result<Gdb> {
        init();
        let trace = record_hello_world();
        let cmd = process::Command::new("rr")
            .arg(trace)
            .arg("--")
            .arg("--quiet")
            .arg("--interpreter=mi3")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()?;
        Ok(Gdb::new(cmd, TIMEOUT))
    }

    #[tokio::test]
    async fn test_stack() -> Result {
        let subject = fixture()?;
        subject
            .break_insert(LineSpec::function("hello_world::HelloMsg::say"))
            .await?;
        subject.exec_run().await?;
        subject.await_stopped(None).await?;

        assert_eq!(2, subject.stack_depth(None).await?);

        let vars = subject.stack_list_variables(false).await?;
        assert_eq!(1, vars.len());
        assert_eq!("self", vars[0].name);
        assert_eq!("*mut hello_world::HelloMsg", vars[0].var_type);
        assert!(vars[0].value.is_some());

        Ok(())
    }

    #[tokio::test]
    async fn test_checkpoint() -> Result {
        let subject = rr_fixture()?;
        subject
            .break_insert(LineSpec::function("hello_world::main"))
            .await?;
        subject.exec_continue().await?;

        let status_at_check = subject.await_stopped(None).await?;
        assert!(matches!(
            &status_at_check.reason,
            &Some(StopReason::Breakpoint { .. })
        ));
        let addr_at_check = status_at_check.address;
        let check = subject.save_checkpoint().await?;
        assert_eq!(Checkpoint(1), check);

        subject.exec_continue().await?;

        subject
            .await_status(|s| matches!(s, &Status::Stopped { .. }), None)
            .await?;

        subject.goto_checkpoint(check).await?;
        assert_eq!(addr_at_check, subject.await_stopped(None).await?.address);

        subject.exec_continue().await?;
        assert_eq!(
            Some(StopReason::SignalReceived),
            subject.await_stopped(None).await?.reason
        );

        Ok(())
    }

    #[tokio::test]
    async fn test_raw_console_for_out() -> Result {
        let subject = fixture()?;

        subject
            .break_insert(LineSpec::function("hello_world::main"))
            .await?;
        subject.exec_run().await?;

        let (resp, lines) = subject.raw_console_cmd_for_output("info locals", 1).await?;
        resp.expect_result()?.expect_msg_is("done")?;
        assert_eq!(vec!["No locals.\\n"], lines);

        Ok(())
    }

    #[tokio::test]
    async fn test_next_status_when_wrong_about_current() -> Result {
        let subject = fixture()?;

        subject.exec_run().await?;
        let status = subject.next_status(Status::Unstarted, None).await?;
        assert_eq!(Status::Running, status);
        Ok(())
    }

    #[tokio::test]
    async fn test_next_status_when_correct_about_current() -> Result {
        let subject = fixture()?;

        subject.exec_run().await?;
        let status = subject.next_status(Status::Running, None).await?;
        assert_eq!(status, Status::Exited(ExitReason::Normal));
        Ok(())
    }

    #[tokio::test]
    async fn test_status_through_break_continue() -> Result {
        let subject = fixture()?;

        let status = subject.status().await?;
        assert_eq!(Status::Unstarted, status);

        subject.break_insert(LineSpec::function("main")).await?;
        subject.exec_run().await?;

        let status = subject.next_status(status, None).await?;
        assert_eq!(Status::Running, status);

        let status = subject.next_status(status, None).await?;
        assert!(matches!(
            &status,
            &Status::Stopped(status::Stopped {
                reason: Some(StopReason::Breakpoint { number }),
                function: Some(ref function),
                ..
            }) if number == 1 && function == "main"
        ));

        subject.exec_continue().await?;

        let status = subject.next_status(status, None).await?;
        assert_eq!(Status::Running, status);

        let status = subject.next_status(status, None).await?;
        assert_eq!(status, Status::Exited(ExitReason::Normal));

        Ok(())
    }

    #[tokio::test]
    async fn test_break() -> Result {
        let subject = fixture()?;

        let bp = subject
            .break_insert(LineSpec::line("samples/hello_world/src/main.rs", 13))
            .await?;
        assert_eq!(1, bp.number);
        assert!(bp
            .file
            .as_ref()
            .unwrap()
            .ends_with("samples/hello_world/src/main.rs"));
        assert_eq!(17, bp.line.unwrap());
        assert_eq!(0, bp.times);

        subject.break_disable(iter::once(&bp)).await?;
        subject.break_delete(iter::once(&bp)).await?;

        Ok(())
    }

    #[tokio::test]
    async fn test_exec_continue() -> Result {
        let subject = fixture()?;
        subject.break_insert(LineSpec::function("main")).await?;
        subject.exec_run().await?;
        subject.exec_continue().await?;
        Ok(())
    }

    #[tokio::test]
    async fn test_exec_continue_not_running() -> Result {
        let subject = fixture()?;
        let error = match subject.exec_continue().await {
            Err(Error::Gdb(error)) => error,
            got => panic!("Expected Error::Gdb, got {:?}", got),
        };
        assert_eq!(error.msg.unwrap(), "The program is not being run.");
        Ok(())
    }

    #[tokio::test]
    async fn test_exec_run() -> Result {
        let subject = fixture()?;
        subject.exec_run().await?;
        Ok(())
    }

    #[tokio::test]
    async fn test_symbol_info_function() -> Result {
        let subject = fixture()?;
        // Convert to BTreeMap so it has stable order
        let symbols: BTreeMap<_, _> = subject.symbol_info_functions().await?.into_iter().collect();
        assert_debug_snapshot!(symbols);
        Ok(())
    }

    #[tokio::test]
    async fn test_await_ready() -> Result {
        let subject = fixture()?;
        subject.await_ready().await?;
        Ok(())
    }

    #[tokio::test]
    async fn test_pop_general() -> Result {
        let subject = fixture()?;
        subject.raw_cmd("-gdb-version").await?;
        let general = subject.pop_general().await?;
        assert!(!general.is_empty());
        Ok(())
    }

    #[tokio::test]
    async fn test_invalid_command() -> Result {
        let subject = fixture()?;

        let err = subject.raw_cmd("-invalid-command").await.unwrap_err();

        assert_eq!(
            Error::Gdb(GdbError {
                code: Some("undefined-command".into()),
                msg: Some("Undefined MI command: invalid-command".into()),
            }),
            err
        );

        Ok(())
    }
}
