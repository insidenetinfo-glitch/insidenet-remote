use super::{
    installer_shell::{
        get_system_executable, path_for_cmd_assignment, path_for_cmd_environment,
        run_elevated_and_wait, trusted_install_environment,
        BATCH_SHORTCUT_DECODE_FAILURE_EXIT_CODE, CMD_RELATIVE_PATH,
    },
    validate_install_app_name, ResultType,
};
use hbb_common::{bail, log};
use std::{
    fs,
    io::{self, Write},
    path::PathBuf,
};

const CHCP_RELATIVE_PATH: &str = "chcp.com";
const UTF8_CODE_PAGE: u32 = 65001;
const BATCH_CODE_PAGE_FAILURE_EXIT_CODE: u32 = 0x5253_0005;
const BATCH_OUTPUT_DIRECTORY_EXISTS_EXIT_CODE: u32 = 0x5253_0006;
const BATCH_OUTPUT_DIRECTORY_CREATE_FAILURE_EXIT_CODE: u32 = 0x5253_0007;

struct InstallCommandScript {
    path: PathBuf,
}

impl Drop for InstallCommandScript {
    fn drop(&mut self) {
        if let Err(err) = fs::remove_file(&self.path) {
            if err.kind() != io::ErrorKind::NotFound {
                log::warn!(
                    "Failed to remove temporary installer file {:?}: {err}",
                    self.path
                );
            }
        }
    }
}

fn prepare_install_commands(commands: &str) -> ResultType<String> {
    let commands = commands.replace("\r\n", "\n").replace('\n', "\r\n");
    let chcp_path = get_system_executable(CHCP_RELATIVE_PATH)?;
    let chcp = path_for_cmd_environment(&chcp_path)?;
    Ok(format!(
        "@echo off\r\nsetlocal EnableExtensions DisableDelayedExpansion\r\n\
         \"{chcp}\" {UTF8_CODE_PAGE} > nul || exit /b \
         {BATCH_CODE_PAGE_FAILURE_EXIT_CODE}\r\n\
         {}\r\n\
         if exist \"%~f0.dir\" exit /b {BATCH_OUTPUT_DIRECTORY_EXISTS_EXIT_CODE}\r\n\
         md \"%~f0.dir\" || exit /b {BATCH_OUTPUT_DIRECTORY_CREATE_FAILURE_EXIT_CODE}\r\n\
         set \"RUSTDESK_OUTPUT_DIR=%~f0.dir\"\r\n{commands}\r\nexit /b 0\r\n",
        trusted_install_environment()?
    ))
}

fn write_install_script(cmds: String) -> ResultType<InstallCommandScript> {
    let directory = std::env::temp_dir();
    path_for_cmd_environment(&directory)?;
    let commands = prepare_install_commands(&cmds)?;
    let path = directory.join(format!(
        "rustdesk_install_{}.bat",
        uuid::Uuid::new_v4().simple()
    ));
    let mut file = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&path)?;
    let script = InstallCommandScript { path };
    file.write_all(commands.as_bytes())?;
    file.sync_all()?;
    Ok(script)
}

pub(super) fn run_cmds(cmds: String, show: bool, tip: &str) -> ResultType<()> {
    validate_install_app_name(&crate::get_app_name())?;
    let script = write_install_script(cmds)?;
    let cmd_path = get_system_executable(CMD_RELATIVE_PATH)?;
    let source = path_for_cmd_assignment(&script.path)?;
    // Run the script directly from its temp location. An earlier revision copied it
    // into a protected system directory and verified its hash via certutil before
    // running it, but certutil's crypto init can stall for a long time on some
    // machines, hanging the whole install. Direct execution avoids that dependency.
    let parameters = format!("/D /E:ON /V:OFF /C \"\"{source}\"\"");
    let exit_code = run_elevated_and_wait(&cmd_path, &parameters, show)?;
    if exit_code != 0 {
        bail!(
            "{tip} failed with elevated exit code {exit_code}: {}",
            elevated_install_failure_reason(exit_code)
        );
    }
    Ok(())
}

fn elevated_install_failure_reason(exit_code: u32) -> &'static str {
    match exit_code {
        BATCH_CODE_PAGE_FAILURE_EXIT_CODE => "failed to set the installer code page",
        BATCH_OUTPUT_DIRECTORY_EXISTS_EXIT_CODE => "installer output directory already exists",
        BATCH_OUTPUT_DIRECTORY_CREATE_FAILURE_EXIT_CODE => {
            "failed to create the installer output directory"
        }
        BATCH_SHORTCUT_DECODE_FAILURE_EXIT_CODE => "failed to decode an embedded shortcut",
        _ => "installer command failed",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn install_script_is_written_and_cleaned_up() {
        let script = write_install_script("> \"%~f0.marker\" echo hi\r\n".to_owned())
            .expect("install script should be created");
        assert!(script.path.exists(), "script file should exist on disk");
        let path = script.path.clone();
        drop(script);
        assert!(!path.exists(), "script file should be removed on drop");
    }
}
