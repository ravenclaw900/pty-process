use crate::error::*;
use crate::pty::Pty as _;

use ::std::os::unix::io::AsRawFd as _;

#[cfg(any(feature = "backend-async-std", feature = "backend-smol"))]
mod async_process;
#[cfg(feature = "backend-std")]
mod std;
#[cfg(feature = "backend-tokio")]
mod tokio;

/// Adds methods to the existing `Command` struct.
///
/// This trait is automatically implemented for a backend's `Command` struct
/// when that backend's feature is enabled.
pub trait Command {
    type Child;
    type Pty;

    /// Creates a new pty, associates the command's stdin/stdout/stderr with
    /// that pty, and then calls `spawn`. This will override any previous
    /// calls to `stdin`/`stdout`/`stderr`.
    fn spawn_pty(
        &mut self,
        size: Option<&crate::pty::Size>,
    ) -> Result<Child<Self::Child, Self::Pty>>;
}

impl<T> Command for T
where
    T: CommandImpl,
    T::Pty: crate::pty::Pty,
    <<T as CommandImpl>::Pty as crate::pty::Pty>::Pt:
        ::std::os::unix::io::AsRawFd,
{
    type Child = T::Child;
    type Pty = T::Pty;

    fn spawn_pty(
        &mut self,
        size: Option<&crate::pty::Size>,
    ) -> Result<Child<Self::Child, Self::Pty>> {
        let (pty, pts, stdin, stdout, stderr) = setup_pty::<Self::Pty>(size)?;
        
        println!("Setup pty");

        let pt_fd = pty.pt().as_raw_fd();
        let pts_fd = pts.as_raw_fd();
        
        println!("Got fds");

        self.std_fds(stdin, stdout, stderr);

        let pre_exec = move || {
            println!("Started pre-exec");
            nix::unistd::setsid().map_err(|e| e.as_errno().unwrap())?;
            set_controlling_terminal(pts_fd)
                .map_err(|e| e.as_errno().unwrap())?;

            // in the parent, destructors will handle closing these file
            // descriptors (other than pt, used by the parent to
            // communicate with the child) when the function ends, but in
            // the child, we end by calling exec(), which doesn't call
            // destructors.

            println!("Closing old fds");
            // XXX unwrap
            nix::unistd::close(pt_fd).map_err(|e| e.as_errno().unwrap())?;
            nix::unistd::close(pts_fd).map_err(|e| e.as_errno().unwrap())?;
            // at this point, stdin/stdout/stderr have already been
            // reopened as fds 0/1/2 in the child, so we can (and should)
            // close the originals
            nix::unistd::close(stdin).map_err(|e| e.as_errno().unwrap())?;
            nix::unistd::close(stdout).map_err(|e| e.as_errno().unwrap())?;
            nix::unistd::close(stderr).map_err(|e| e.as_errno().unwrap())?;
            println!("Closed old fds");

            Ok(())
        };
        // safe because setsid() and close() are async-signal-safe functions
        // and ioctl() is a raw syscall (which is inherently
        // async-signal-safe).
        unsafe { self.pre_exec_impl(pre_exec) };

        let child = self.spawn_impl().map_err(Error::Spawn)?;

        Ok(Child { child, pty })
    }
}

/// Wrapper struct adding pty methods to the normal `Child` struct.
pub struct Child<C, P> {
    child: C,
    pty: P,
}

impl<C, P> Child<C, P>
where
    P: crate::pty::Pty,
{
    /// Returns a reference to the pty.
    ///
    /// The underlying pty instance is guaranteed to implement
    /// [`AsRawFd`](::std::os::unix::io::AsRawFd), as well as the appropriate
    /// `Read` and `Write` traits for the associated backend.
    pub fn pty(&self) -> &P::Pt {
        self.pty.pt()
    }

    /// Returns a mutable reference to the pty.
    ///
    /// The underlying pty instance is guaranteed to implement
    /// [`AsRawFd`](::std::os::unix::io::AsRawFd), as well as the appropriate
    /// `Read` and `Write` traits for the associated backend.
    ///
    /// This method is primarily useful for the tokio backend, since tokio's
    /// `AsyncRead` and `AsyncWrite` traits have methods which take mutable
    /// references.
    pub fn pty_mut(&mut self) -> &mut P::Pt {
        self.pty.pt_mut()
    }

    /// Causes the pty to change its size.
    ///
    /// This will additionally cause a `SIGWINCH` signal to be sent to the
    /// running process.
    pub fn resize_pty(&self, size: &crate::pty::Size) -> Result<()> {
        self.pty.resize(size)
    }
}

impl<C, P> ::std::ops::Deref for Child<C, P> {
    type Target = C;

    fn deref(&self) -> &Self::Target {
        &self.child
    }
}

impl<C, P> ::std::ops::DerefMut for Child<C, P> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.child
    }
}

// XXX shouldn't be pub?
pub trait CommandImpl {
    type Child;
    type Pty;

    fn std_fds(
        &mut self,
        stdin: ::std::os::unix::io::RawFd,
        stdout: ::std::os::unix::io::RawFd,
        stderr: ::std::os::unix::io::RawFd,
    );
    unsafe fn pre_exec_impl<F>(&mut self, f: F)
    where
        F: FnMut() -> ::std::io::Result<()> + Send + Sync + 'static;
    fn spawn_impl(&mut self) -> ::std::io::Result<Self::Child>;
}

fn setup_pty<P>(
    size: Option<&crate::pty::Size>,
) -> Result<(
    P,
    ::std::fs::File,
    ::std::os::unix::io::RawFd,
    ::std::os::unix::io::RawFd,
    ::std::os::unix::io::RawFd,
)>
where
    P: crate::pty::Pty,
{
    let pty = P::new()?;
    if let Some(size) = size {
        pty.resize(size)?;
    }

    let pts = pty.pts()?;
    let pts_fd = pts.as_raw_fd();

    let stdin = nix::unistd::dup(pts_fd).map_err(Error::SpawnNix)?;
    let stdout = nix::unistd::dup(pts_fd).map_err(Error::SpawnNix)?;
    let stderr = nix::unistd::dup(pts_fd).map_err(Error::SpawnNix)?;

    Ok((pty, pts, stdin, stdout, stderr))
}

fn set_controlling_terminal(
    fd: ::std::os::unix::io::RawFd,
) -> nix::Result<()> {
    // safe because std::fs::File is required to contain a valid file
    // descriptor
    unsafe { set_controlling_terminal_unsafe(fd, ::std::ptr::null()) }
        .map(|_| ())
}

nix::ioctl_write_ptr_bad!(
    set_controlling_terminal_unsafe,
    libc::TIOCSCTTY,
    libc::c_int
);
