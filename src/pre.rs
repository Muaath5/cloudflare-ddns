use crate::err;

#[cfg(target_os = "linux")]
fn ensure_root() {
    use nix::unistd::Uid;
    use std::convert::Infallible;
    use std::io;
    use std::os::unix::process::CommandExt;

    if !Uid::effective().is_root() {
        fn elevate() -> io::Result<Infallible> {
            let err = std::process::Command::new("sudo")
                // contains exe path
                .args(std::env::args_os())
                .exec();
            Err(err)
        }

        elevate().unwrap_or_else(|e| crate::abort!("{e}"));
    }
}

fn set_working_dir() {
    #[cfg(not(feature = "dev-build"))]
    {
        use std::env;

        env::current_exe()
            .and_then(|mut path| {
                path.pop();
                env::set_current_dir(&path)
            })
            .unwrap_or_else(|e| crate::abort!("{e}"));
    }
}

pub fn pre_run() {
    err::set_hook();
    #[cfg(target_os = "linux")]
    ensure_root();

    set_working_dir();
}
