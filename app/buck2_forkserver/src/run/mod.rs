/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is licensed under both the MIT license found in the
 * LICENSE-MIT file in the root directory of this source tree and the Apache
 * License, Version 2.0 found in the LICENSE-APACHE file in the root directory
 * of this source tree.
 */

mod interruptible_async_read;

use std::io;
use std::pin::Pin;
use std::process::Command;
use std::process::ExitStatus;
use std::process::Stdio;
use std::task::Context;
use std::task::Poll;
use std::time::Duration;

use anyhow::Context as _;
use bytes::Bytes;
use futures::future::Future;
use futures::future::FutureExt;
use futures::stream::Stream;
use futures::stream::StreamExt;
use futures::stream::TryStreamExt;
use pin_project::pin_project;
use tokio::process::Child;
use tokio_util::codec::BytesCodec;
use tokio_util::codec::FramedRead;

use self::interruptible_async_read::InterruptNotifiable;
use self::interruptible_async_read::InterruptibleAsyncRead;

#[derive(Debug)]
pub enum GatherOutputStatus {
    Finished(ExitStatus),
    TimedOut(Duration),
    Cancelled,
}

#[derive(Debug)]
pub enum CommandEvent {
    Stdout(Bytes),
    Stderr(Bytes),
    Exit(GatherOutputStatus),
}

enum StdioEvent {
    Stdout(Bytes),
    Stderr(Bytes),
}

impl From<StdioEvent> for CommandEvent {
    fn from(stdio: StdioEvent) -> Self {
        match stdio {
            StdioEvent::Stdout(bytes) => CommandEvent::Stdout(bytes),
            StdioEvent::Stderr(bytes) => CommandEvent::Stderr(bytes),
        }
    }
}

/// This stream will yield [CommandEvent] whenever we have something on stdout or stderr (this is
/// our stdio stream), and it'll finish up the stream with the exit status. This is basically like
/// a select, but with the exit guaranteed to come last.
#[pin_project]
struct CommandEventStream<Status, Stdio> {
    exit: Option<anyhow::Result<GatherOutputStatus>>,

    done: bool,

    #[pin]
    status: futures::future::Fuse<Status>,

    #[pin]
    stdio: futures::stream::Fuse<Stdio>,
}

impl<Status, Stdio> CommandEventStream<Status, Stdio>
where
    Status: Future,
    Stdio: Stream,
{
    fn new(status: Status, stdio: Stdio) -> Self {
        Self {
            exit: None,
            done: false,
            status: status.fuse(),
            stdio: stdio.fuse(),
        }
    }
}

impl<Status, Stdio> Stream for CommandEventStream<Status, Stdio>
where
    Status: Future<Output = anyhow::Result<GatherOutputStatus>>,
    Stdio: Stream<Item = anyhow::Result<StdioEvent>> + InterruptNotifiable,
{
    type Item = anyhow::Result<CommandEvent>;

    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let mut this = self.project();

        if *this.done {
            return Poll::Ready(None);
        }

        // This future is fused so it's guaranteed to be ready once. If it does, capture the exit
        // status, we'll return it later.
        if let Poll::Ready(status) = this.status.poll(cx) {
            *this.exit = Some(status);
            this.stdio.as_mut().get_pin_mut().notify_interrupt();
        }

        // This stram is also fused, so if it returns None, we'll know it's done for good and we'll
        // return the exit status if it's available.
        if let Some(stdio) = futures::ready!(this.stdio.poll_next(cx)) {
            return Poll::Ready(Some(stdio.map(|event| event.into())));
        }

        // If we got here that means the stream is done. If we have it we return, and if we don't
        // we report we're pending, because we'll have polled it already earlier.
        if let Some(exit) = this.exit.take() {
            *this.done = true;
            return Poll::Ready(Some(exit.map(CommandEvent::Exit)));
        }

        Poll::Pending
    }
}

pub async fn timeout_into_cancellation(
    timeout: Option<Duration>,
) -> anyhow::Result<GatherOutputStatus> {
    match timeout {
        Some(t) => {
            tokio::time::sleep(t).await;
            Ok(GatherOutputStatus::TimedOut(t))
        }
        None => futures::future::pending().await,
    }
}

pub fn stream_command_events<T>(
    mut child: Child,
    cancellation: T,
) -> anyhow::Result<impl Stream<Item = anyhow::Result<CommandEvent>>>
where
    T: Future<Output = anyhow::Result<GatherOutputStatus>>,
{
    let stdout = child.stdout.take().context("Child stdout is not piped")?;
    let stderr = child.stderr.take().context("Child stderr is not piped")?;

    #[cfg(unix)]
    type Drainer<R> = self::interruptible_async_read::UnixNonBlockingDrainer<R>;

    // On Windows, for the time being we just give ourselves a timeout to finish reading.
    // Ideally this would perform a non-blocking read on self instead like we do on Unix.
    #[cfg(not(unix))]
    type Drainer<R> = self::interruptible_async_read::TimeoutDrainer<R>;

    let stdout = InterruptibleAsyncRead::<_, Drainer<_>>::new(stdout);
    let stderr = InterruptibleAsyncRead::<_, Drainer<_>>::new(stderr);

    let status = async move {
        let (result, cancelled) = {
            let wait = async {
                let status = GatherOutputStatus::Finished(child.wait().await?);
                anyhow::Ok((status, false))
            };

            let cancellation = async {
                let status = cancellation.await?;
                anyhow::Ok((status, true))
            };

            futures::pin_mut!(wait);
            futures::pin_mut!(cancellation);

            futures::future::select(wait, cancellation)
                .await
                .factor_first()
                .0
        }?;

        if cancelled {
            kill_process(&child).context("Failed to terminate child after timeout")?;
        }

        Ok(result)
    };

    let stdout = FramedRead::new(stdout, BytesCodec::new())
        .map(|data| anyhow::Ok(StdioEvent::Stdout(data?.freeze())));
    let stderr = FramedRead::new(stderr, BytesCodec::new())
        .map(|data| anyhow::Ok(StdioEvent::Stderr(data?.freeze())));

    let stdio = futures::stream::select(stdout, stderr);

    Ok(CommandEventStream::new(status, stdio))
}

pub(crate) async fn decode_command_event_stream<S>(
    stream: S,
) -> anyhow::Result<(GatherOutputStatus, Vec<u8>, Vec<u8>)>
where
    S: Stream<Item = anyhow::Result<CommandEvent>>,
{
    futures::pin_mut!(stream);

    let mut stdout = Vec::<u8>::new();
    let mut stderr = Vec::<u8>::new();

    while let Some(event) = stream.try_next().await? {
        match event {
            CommandEvent::Stdout(bytes) => stdout.extend(&bytes),
            CommandEvent::Stderr(bytes) => stderr.extend(&bytes),
            CommandEvent::Exit(exit) => return Ok((exit, stdout, stderr)),
        }
    }

    Err(anyhow::Error::msg(
        "Stream did not yield CommandEvent::Exit",
    ))
}

pub async fn gather_output<T>(
    cmd: Command,
    cancellation: T,
) -> anyhow::Result<(GatherOutputStatus, Vec<u8>, Vec<u8>)>
where
    T: Future<Output = anyhow::Result<GatherOutputStatus>> + Send,
{
    let cmd = prepare_command(cmd);

    let child = spawn_retry_txt_busy(cmd, || tokio::time::sleep(Duration::from_millis(50)))
        .await
        .context("Failed to start command")?;

    let stream = stream_command_events(child, cancellation)?;
    decode_command_event_stream(stream).await
}

fn kill_process(child: &Child) -> anyhow::Result<()> {
    let pid = match child.id() {
        Some(pid) => pid,
        None => {
            // Child just exited, so in this case we don't want to kill anything.
            return Ok(());
        }
    };
    tracing::info!("Killing process {}", pid);
    kill_process_impl(pid)
}

#[cfg(unix)]
fn kill_process_impl(pid: u32) -> anyhow::Result<()> {
    use nix::sys::signal;
    use nix::sys::signal::Signal;
    use nix::unistd::Pid;

    let pid: i32 = pid.try_into().context("PID does not fit a i32")?;

    signal::killpg(Pid::from_raw(pid), Signal::SIGKILL)
        .with_context(|| format!("Failed to kill process {}", pid))
}

#[cfg(windows)]
fn kill_process_impl(pid: u32) -> anyhow::Result<()> {
    use winapi::um::handleapi::CloseHandle;
    use winapi::um::processthreadsapi::OpenProcess;
    use winapi::um::processthreadsapi::TerminateProcess;
    use winapi::um::winnt::PROCESS_TERMINATE;

    let proc_handle = unsafe { OpenProcess(PROCESS_TERMINATE, 0, pid) };
    // If proc_handle is null, proccess died already.
    if proc_handle.is_null() {
        return Ok(());
    }
    let terminate_res = unsafe { TerminateProcess(proc_handle, 1) };
    unsafe { CloseHandle(proc_handle) };
    match terminate_res {
        0 => Err(anyhow::anyhow!("Failed to kill process {}", pid)),
        _ => Ok(()),
    }
}

pub fn prepare_command(mut cmd: Command) -> tokio::process::Command {
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        cmd.process_group(0);
    }

    cmd.stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    cmd.into()
}

/// fork-exec is a bit tricky in a busy process. We often have files open to writing just prior to
/// executing them (as we download from RE), and many processes being spawned concurrently. We do
/// close the fds properly before the exec, but what can happn is:
///
/// - Some thread forks
/// - We close the file. At this time we don't have it open, but the forked process does.
/// - We try to exec the file. This fails because the file is open for writing (by the forked
/// process).
/// - The forked process execs. At this point the file is closed (because everything is CLOEXEC).
///
/// The window during which the forked process holds the fd is small, so retrying a couple times
/// here should let us make this work.
///
/// The more correct solution for this here would be to start a fork server in a separate process
/// when we start.  However, until we get there, this should do the trick.
async fn spawn_retry_txt_busy<F, D>(
    mut cmd: tokio::process::Command,
    mut delay: F,
) -> io::Result<Child>
where
    F: FnMut() -> D,
    D: Future<Output = ()>,
{
    let mut attempts = 10;

    loop {
        let res = cmd.spawn();

        let res_errno = res.as_ref().map_err(|e| e.raw_os_error());
        let is_txt_busy = matches!(res_errno, Err(Some(libc::ETXTBSY)));

        if attempts == 0 || !is_txt_busy {
            return res;
        }

        delay().await;

        attempts -= 1;
    }
}

#[cfg(test)]
mod tests {
    use std::str;
    use std::time::Instant;

    use assert_matches::assert_matches;
    use buck2_core::process::async_background_command;
    use buck2_core::process::background_command;

    use super::*;

    #[tokio::test]
    async fn test_gather_output() -> anyhow::Result<()> {
        let mut cmd = if cfg!(windows) {
            background_command("powershell")
        } else {
            background_command("sh")
        };
        cmd.args(["-c", "echo hello"]);

        let (status, stdout, stderr) = gather_output(cmd, futures::future::pending()).await?;
        assert!(matches!(status, GatherOutputStatus::Finished(s) if s.code() == Some(0)));
        assert_eq!(str::from_utf8(&stdout)?.trim(), "hello");
        assert_eq!(stderr, b"");

        Ok(())
    }

    #[tokio::test]
    async fn test_gather_does_not_wait_for_children() -> anyhow::Result<()> {
        // If we wait for sleep, this will time out.
        let mut cmd = if cfg!(windows) {
            background_command("powershell")
        } else {
            background_command("sh")
        };
        if cfg!(windows) {
            cmd.args([
                "-c",
                "Start-Job -ScriptBlock {sleep 10} | Out-Null; echo hello",
            ]);
        } else {
            cmd.args(["-c", "(sleep 10 &) && echo hello"]);
        }

        let timeout = if cfg!(windows) { 5 } else { 1 };
        let (status, stdout, stderr) = gather_output(
            cmd,
            timeout_into_cancellation(Some(Duration::from_secs(timeout))),
        )
        .await?;
        assert!(matches!(status, GatherOutputStatus::Finished(s) if s.code() == Some(0)));
        assert_eq!(str::from_utf8(&stdout)?.trim(), "hello");
        assert_eq!(stderr, b"");

        Ok(())
    }

    #[tokio::test]
    async fn test_gather_output_timeout() -> anyhow::Result<()> {
        let now = Instant::now();

        let mut cmd = if cfg!(windows) {
            background_command("powershell")
        } else {
            background_command("sh")
        };
        cmd.args(["-c", "echo hello; sleep 10; echo bye"]);

        let timeout = if cfg!(windows) { 5 } else { 1 };
        let (status, stdout, stderr) = gather_output(
            cmd,
            timeout_into_cancellation(Some(Duration::from_secs(timeout))),
        )
        .await?;
        assert!(matches!(status, GatherOutputStatus::TimedOut(..)));
        assert_eq!(str::from_utf8(&stdout)?.trim(), "hello");
        assert_eq!(stderr, b"");

        assert!(now.elapsed() < Duration::from_secs(9)); // Lots of leeway here.

        Ok(())
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn test_spawn_retry_txt_busy() -> anyhow::Result<()> {
        use futures::future;
        use tokio::fs::OpenOptions;
        use tokio::io::AsyncWriteExt;

        let tempdir = tempfile::tempdir()?;
        let bin = tempdir.path().join("bin");

        let mut file = OpenOptions::new()
            .mode(0o755)
            .write(true)
            .create(true)
            .open(&bin)
            .await?;

        file.write_all(b"#!/bin/bash\ntrue\n").await?;

        let cmd = async_background_command(&bin);
        let mut child = spawn_retry_txt_busy(cmd, {
            let mut file = Some(file);
            move || {
                file.take();
                future::ready(())
            }
        })
        .await?;

        let status = child.wait().await?;
        assert_eq!(status.code(), Some(0));

        Ok(())
    }

    #[tokio::test]
    async fn test_spawn_retry_other_error() -> anyhow::Result<()> {
        let tempdir = tempfile::tempdir()?;
        let bin = tempdir.path().join("bin"); // Does not actually exist

        let cmd = async_background_command(&bin);
        let res = spawn_retry_txt_busy(cmd, || async { panic!("Should not be called!") }).await;
        assert!(res.is_err());

        Ok(())
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn test_kill_terminates_process_group() -> anyhow::Result<()> {
        use std::str::FromStr;

        use nix::errno::Errno;
        use nix::sys::signal;
        use nix::unistd::Pid;

        // This command will spawn 2 subprocesses (subshells) and print the PID of the 2nd shell.
        let mut cmd = background_command("sh");
        cmd.arg("-c").arg("( ( echo $$ && sleep 1000 ) )");
        let (_status, stdout, _stderr) =
            gather_output(cmd, timeout_into_cancellation(Some(Duration::from_secs(1)))).await?;
        let pid = i32::from_str(std::str::from_utf8(&stdout)?.trim())?;

        for _ in 0..10 {
            // This does rely on no PID reuse but the odds of PIDs wrapping around all the way to the
            // same PID we just used before we issue this kill seem low. So, we expect this to error
            // out.
            if matches!(signal::kill(Pid::from_raw(pid), None), Err(e) if e == Errno::ESRCH) {
                return Ok(());
            }

            // This is awkward but unfortunately the process does not immediately disappear.
            tokio::time::sleep(Duration::from_secs(1)).await;
        }

        Err(anyhow::anyhow!("PID did not exit: {}", pid))
    }

    #[tokio::test]
    async fn test_stream_command_events_ends() -> anyhow::Result<()> {
        let mut cmd = if cfg!(windows) {
            background_command("powershell")
        } else {
            background_command("sh")
        };
        cmd.args(["-c", "exit 0"]);

        let child = prepare_command(cmd).spawn()?;
        let mut events = stream_command_events(child, futures::future::pending())?.boxed();
        assert_matches!(events.next().await, Some(Ok(CommandEvent::Exit(..))));
        assert_matches!(futures::poll!(events.next()), Poll::Ready(None));
        Ok(())
    }
}
