use crate::{
    failure::{system_error, Failure},
    format::CodeStr,
    spinner::spin,
};
use std::{
    fs::{create_dir_all, metadata, rename},
    io,
    io::{Read, Write},
    path::{Path, PathBuf},
    process::{ChildStdin, Command, Stdio},
    string::ToString,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
};
use tempfile::tempdir;
use uuid::Uuid;
use walkdir::WalkDir;

// Construct a random image tag.
pub fn random_tag() -> String {
    Uuid::new_v4()
        .to_simple()
        .encode_lower(&mut Uuid::encode_buffer())
        .to_owned()
}

// Query whether an image exists locally.
pub fn image_exists(image: &str, interrupted: &Arc<AtomicBool>) -> Result<bool, Failure> {
    debug!("Checking existence of image {}\u{2026}", image.code_str());

    match run_quiet(
        "Checking existence of image\u{2026}",
        "The image doesn't exist.",
        &["image", "inspect", image],
        interrupted,
    ) {
        Ok(_) => Ok(true),
        Err(Failure::Interrupted) => Err(Failure::Interrupted),
        Err(Failure::System(_, _)) | Err(Failure::User(_, _)) => Ok(false),
    }
}

// Push an image.
pub fn push_image(image: &str, interrupted: &Arc<AtomicBool>) -> Result<(), Failure> {
    debug!("Pushing image {}\u{2026}", image.code_str());

    run_quiet(
        "Pushing image\u{2026}",
        "Unable to push image.",
        &["image", "push", image],
        interrupted,
    )
    .map(|_| ())
}

// Pull an image.
pub fn pull_image(image: &str, interrupted: &Arc<AtomicBool>) -> Result<(), Failure> {
    debug!("Pulling image {}\u{2026}", image.code_str());

    run_quiet(
        "Pulling image\u{2026}",
        "Unable to pull image.",
        &["image", "pull", image],
        interrupted,
    )
    .map(|_| ())
}

// Delete an image.
pub fn delete_image(image: &str, interrupted: &Arc<AtomicBool>) -> Result<(), Failure> {
    debug!("Deleting image {}\u{2026}", image.code_str());

    run_quiet(
        "Deleting image\u{2026}",
        "Unable to delete image.",
        &["image", "rm", "--force", image],
        interrupted,
    )
    .map(|_| ())
}

// Create a container and return its ID.
pub fn create_container(
    image: &str,
    ports: &[String],
    interrupted: &Arc<AtomicBool>,
) -> Result<String, Failure> {
    debug!("Creating container from image {}\u{2026}", image.code_str(),);

    // Why `--init`? (1) PID 1 is supposed to reap orphaned zombie processes, otherwise they can
    // accumulate. Bash does this, but we run `/bin/sh` in the container, which may or may not be
    // Bash. So `--init` runs Tini (https://github.com/krallin/tini) as PID 1, which properly reaps
    // orphaned zombies. (2) PID 1 also does not exhibit the default behavior (crashing) for signals
    // like SIGINT and SIGTERM. However, PID 1 can still handle these signals by explicitly trapping
    // them. Tini traps these signals and forwards them to the child process. Then the default
    // signal handling behavior of the child process (in our case, `/bin/sh`) works normally.
    // [tag:--init]
    let mut command = vec!["container", "create", "--init", "--interactive"];

    for port in ports {
        command.extend(vec!["--publish", port]);
    }

    command.extend(vec![image, "/bin/sh"]);

    Ok(run_quiet(
        "Creating container\u{2026}",
        "Unable to create container.",
        &command,
        interrupted,
    )?
    .trim()
    .to_owned())
}

// Copy files into a container.
pub fn copy_into_container<R: Read>(
    container: &str,
    mut tar: R,
    interrupted: &Arc<AtomicBool>,
) -> Result<(), Failure> {
    debug!(
        "Copying files into container {}\u{2026}",
        container.code_str()
    );

    run_quiet_stdin(
        "Copying files into container\u{2026}",
        "Unable to copy files into the container.",
        &["container", "cp", "-", &format!("{}:{}", container, "/")],
        |mut stdin| {
            io::copy(&mut tar, &mut stdin)
                .map_err(system_error("Unable to copy files into the container."))?;

            Ok(())
        },
        interrupted,
    )
    .map(|_| ())
}

// Copy files from a container.
pub fn copy_from_container(
    container: &str,
    paths: &[PathBuf],
    source_dir: &Path,
    destination_dir: &Path,
    interrupted: &Arc<AtomicBool>,
) -> Result<(), Failure> {
    // Copy each path from the container to the host.
    for path in paths {
        debug!(
            "Copying {} from container {}\u{2026}",
            path.to_string_lossy().code_str(),
            container.code_str()
        );

        // `docker container cp` is not idempotent. For example, suppose there is a directory called
        // `/foo` in the container and `/bar` does not exist on the host. Consider the following
        // command:
        //   `docker cp container:/foo /bar`
        // The first time that command is run, Docker will create the directory `/bar` on the host
        // and copy the files from `/foo` into it. But if you run it again, Docker will copy `/bar`
        // into the directory `/foo`, resulting in `/foo/foo`, which is undesirable. To work around
        // this, we first copy the path from the container into a temporary directory (where the
        // target path is guaranteed to not exist). Then we copy/move that to the final destination.
        let temp_dir = tempdir().map_err(system_error("Unable to create temporary directory."))?;

        // Figure out what needs to go where.
        let source = source_dir.join(path);
        let intermediate = temp_dir.path().join("data");
        let destination = destination_dir.join(path);

        // Get the path from the container.
        run_quiet(
            "Copying files from the container\u{2026}",
            "Unable to copy files from the container.",
            &[
                "container",
                "cp",
                &format!("{}:{}", container, source.to_string_lossy()),
                &intermediate.to_string_lossy(),
            ],
            interrupted,
        )
        .map(|_| ())?;

        // Check if what we got from the container is a file or a directory.
        if metadata(&intermediate)
            .map_err(system_error(&format!(
                "Unable to retrieve filesystem metadata for path {}.",
                intermediate.to_string_lossy().code_str(),
            )))?
            .is_file()
        {
            // It's a file. Determine the destination directory. The `unwrap` is safe because the
            // root of the filesystem cannot be a file.
            let destination_dir = destination.parent().unwrap().to_owned();

            // Make sure the destination directory exists.
            create_dir_all(&destination_dir).map_err(system_error(&format!(
                "Unable to create directory {}.",
                destination_dir.to_string_lossy().code_str(),
            )))?;

            // Move it to the destination.
            rename(&intermediate, &destination).map_err(system_error(&format!(
                "Unable to move file {} to destination {}.",
                intermediate.to_string_lossy().code_str(),
                destination.to_string_lossy().code_str(),
            )))?;
        } else {
            // It's a directory. Traverse it.
            for entry in WalkDir::new(&intermediate) {
                // If we run into an error traversing the filesystem, report it.
                let entry = entry.map_err(system_error(&format!(
                    "Unable to traverse directory {}.",
                    intermediate.to_string_lossy().code_str(),
                )))?;

                // Figure out what needs to go where. The `unwrap` is safe because `entry` is
                // guaranteed to be inside `intermediate` (or equal to it).
                let entry_path = entry.path();
                let destination_path =
                    destination.join(entry_path.strip_prefix(&intermediate).unwrap());

                // Check if the current entry is a file or a directory.
                if entry.file_type().is_dir() {
                    // It's a directory. Create a directory at the destination.
                    create_dir_all(&destination_path).map_err(system_error(&format!(
                        "Unable to create directory {}.",
                        destination_path.to_string_lossy().code_str(),
                    )))?;
                } else {
                    // It's a file. Move it to the destination.
                    rename(entry_path, &destination_path).map_err(system_error(&format!(
                        "Unable to move file {} to destination {}.",
                        entry_path.to_string_lossy().code_str(),
                        destination_path.to_string_lossy().code_str(),
                    )))?;
                }
            }
        }
    }

    Ok(())
}

// Start a container.
pub fn start_container(
    container: &str,
    command: &str,
    interrupted: &Arc<AtomicBool>,
) -> Result<(), Failure> {
    debug!("Starting container {}\u{2026}", container.code_str());

    run_loud_stdin(
        "Unable to start container.",
        &["container", "start", "--attach", "--interactive", container],
        |stdin| {
            write!(stdin, "{}", command).map_err(system_error(&format!(
                "Unable to send command {} to the container.",
                command.code_str(),
            )))?;

            Ok(())
        },
        interrupted,
    )
    .map(|_| ())
}

// Stop a container.
pub fn stop_container(container: &str, interrupted: &Arc<AtomicBool>) -> Result<(), Failure> {
    debug!("Stopping container {}\u{2026}", container.code_str());

    run_quiet(
        "Stopping container\u{2026}",
        "Unable to stop container.",
        &["container", "stop", container],
        interrupted,
    )
    .map(|_| ())
}

// Commit a container to an image.
pub fn commit_container(
    container: &str,
    image: &str,
    interrupted: &Arc<AtomicBool>,
) -> Result<(), Failure> {
    debug!(
        "Committing container {} to image {}\u{2026}",
        container.code_str(),
        image.code_str()
    );

    run_quiet(
        "Committing container\u{2026}",
        "Unable to commit container.",
        &["container", "commit", container, image],
        interrupted,
    )
    .map(|_| ())
}

// Delete a container.
pub fn delete_container(container: &str, interrupted: &Arc<AtomicBool>) -> Result<(), Failure> {
    debug!("Deleting container {}\u{2026}", container.code_str());

    run_quiet(
        "Deleting container\u{2026}",
        "Unable to delete container.",
        &["container", "rm", "--force", container],
        interrupted,
    )
    .map(|_| ())
}

// Run an interactive shell.
pub fn spawn_shell(image: &str, interrupted: &Arc<AtomicBool>) -> Result<(), Failure> {
    debug!(
        "Spawning an interactive shell for image {}\u{2026}",
        image.code_str()
    );

    run_attach(
        "The shell exited with a failure.",
        &[
            "container",
            "run",
            "--rm",
            "--interactive",
            "--tty",
            "--init", // [ref:--init]
            image,
            "/bin/su", // We use `su` rather than `sh` to use the root user's shell.
        ],
        interrupted,
    )
}

// Run a command, forward its standard error stream, and return its standard output.
fn run_quiet(
    spinner_message: &str,
    error: &str,
    args: &[&str],
    interrupted: &Arc<AtomicBool>,
) -> Result<String, Failure> {
    // Render a spinner animation and clear it when we're done.
    let _guard = spin(spinner_message);

    // This is used to determine whether the user interrupted the program during the execution of
    // the child process.
    let was_interrupted = interrupted.load(Ordering::SeqCst);

    // Run the child process.
    let output = command(args)
        .stdin(Stdio::null())
        .output()
        .map_err(system_error(&format!(
            "{} Perhaps you don't have Docker installed.",
            error
        )))?;

    // Handle the result.
    if output.status.success() {
        Ok(String::from_utf8_lossy(&output.stdout).to_string())
    } else {
        Err(
            if output.status.code().is_none()
                || (!was_interrupted && interrupted.load(Ordering::SeqCst))
            {
                interrupted.store(true, Ordering::SeqCst);
                Failure::Interrupted
            } else {
                Failure::System(
                    format!(
                        "{} Details:\n{}",
                        error,
                        String::from_utf8_lossy(&output.stderr)
                    ),
                    None,
                )
            },
        )
    }
}

// Run a command, forward its standard error stream, and return its standard output. Accepts a
// closure which receives a pipe to the standard input stream of the child process.
fn run_quiet_stdin<W: FnOnce(&mut ChildStdin) -> Result<(), Failure>>(
    spinner_message: &str,
    error: &str,
    args: &[&str],
    writer: W,
    interrupted: &Arc<AtomicBool>,
) -> Result<String, Failure> {
    // Render a spinner animation and clear it when we're done.
    let _guard = spin(spinner_message);

    // This is used to determine whether the user interrupted the program during the execution of
    // the child process.
    let was_interrupted = interrupted.load(Ordering::SeqCst);

    // Run the child process.
    let mut child = command(args)
        .stdin(Stdio::piped()) // [tag:run_quiet_stdin_piped]
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(system_error(&format!(
            "{} Perhaps you don't have Docker installed.",
            error
        )))?;

    // Pipe data to the child's standard input stream.
    writer(child.stdin.as_mut().unwrap())?; // [ref:run_quiet_stdin_piped]

    // Wait for the child to terminate.
    let output = child.wait_with_output().map_err(system_error(&format!(
        "{} Perhaps you don't have Docker installed.",
        error
    )))?;

    // Handle the result.
    if output.status.success() {
        Ok(String::from_utf8_lossy(&output.stdout).to_string())
    } else {
        Err(
            if output.status.code().is_none()
                || (!was_interrupted && interrupted.load(Ordering::SeqCst))
            {
                interrupted.store(true, Ordering::SeqCst);
                Failure::Interrupted
            } else {
                Failure::System(
                    format!(
                        "{} Details:\n{}",
                        error,
                        String::from_utf8_lossy(&output.stderr)
                    ),
                    None,
                )
            },
        )
    }
}

// Run a command and forward its standard output and error streams. Accepts a closure which receives
// a pipe to the standard input stream of the child process.
fn run_loud_stdin<W: FnOnce(&mut ChildStdin) -> Result<(), Failure>>(
    error: &str,
    args: &[&str],
    writer: W,
    interrupted: &Arc<AtomicBool>,
) -> Result<(), Failure> {
    // This is used to determine whether the user interrupted the program during the execution of
    // the child process.
    let was_interrupted = interrupted.load(Ordering::SeqCst);

    // Run the child process.
    let mut child = command(args)
        .stdin(Stdio::piped()) // [tag:run_loud_stdin_piped]
        .spawn()
        .map_err(system_error(&format!(
            "{} Perhaps you don't have Docker installed.",
            error
        )))?;

    // Pipe data to the child's standard input stream.
    writer(child.stdin.as_mut().unwrap())?; // [ref:run_loud_stdin_piped]

    // Wait for the child to terminate.
    let status = child.wait().map_err(system_error(&format!(
        "{} Perhaps you don't have Docker installed.",
        error
    )))?;

    // Handle the result.
    if status.success() {
        Ok(())
    } else {
        Err(
            if status.code().is_none() || (!was_interrupted && interrupted.load(Ordering::SeqCst)) {
                interrupted.store(true, Ordering::SeqCst);
                Failure::Interrupted
            } else {
                Failure::System(error.to_owned(), None)
            },
        )
    }
}

// Run a command and forward its standard input, output, and error streams.
fn run_attach(error: &str, args: &[&str], interrupted: &Arc<AtomicBool>) -> Result<(), Failure> {
    // This is used to determine whether the user interrupted the program during the execution of
    // the child process.
    let was_interrupted = interrupted.load(Ordering::SeqCst);

    // Run the child process.
    let status = command(args).status().map_err(system_error(&format!(
        "{} Perhaps you don't have Docker installed.",
        error
    )))?;

    // Handle the result.
    if status.success() {
        Ok(())
    } else {
        Err(
            if status.code().is_none() || (!was_interrupted && interrupted.load(Ordering::SeqCst)) {
                interrupted.store(true, Ordering::SeqCst);
                Failure::Interrupted
            } else {
                Failure::System(error.to_owned(), None)
            },
        )
    }
}

// Construct a Docker `Command` from an array of arguments.
fn command(args: &[&str]) -> Command {
    let mut command = Command::new("docker");
    for arg in args {
        command.arg(arg);
    }
    command
}

#[cfg(test)]
mod tests {
    use crate::docker::random_tag;

    #[test]
    fn random_impure() {
        assert_ne!(random_tag(), random_tag());
    }
}
