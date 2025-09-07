use std::process::{Command, Stdio};
use crate::opts::Nsenter;
use crate::error::{Result, NriError};

#[derive(Clone, Debug)]
pub enum Runner {
    Local,
    Nsenter(Nsenter),
}

impl Runner {
    pub fn run_capture(&self, program: &str, args: &[&str]) -> Result<(i32, String, String)> {
        let (prog, argv) = match self {
            Runner::Local => (program.to_string(), args.iter().map(|s| s.to_string()).collect::<Vec<_>>()),
            Runner::Nsenter(ns) => {
                let mut argv = vec!["--target", "1", "--mount", "--uts", "--ipc", "--net", "--pid", "--", program];
                argv.extend(args);
                (ns.path.clone(), argv.iter().map(|s| s.to_string()).collect::<Vec<_>>())
            }
        };

        let output = Command::new(prog)
            .args(argv)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .output()?;
        let code = output.status.code().unwrap_or(-1);
        let out = String::from_utf8_lossy(&output.stdout).to_string();
        let err = String::from_utf8_lossy(&output.stderr).to_string();
        Ok((code, out, err))
    }

    pub fn run_ok(&self, program: &str, args: &[&str]) -> Result<String> {
        let (code, out, err) = self.run_capture(program, args)?;
        if code == 0 {
            Ok(out)
        } else {
            Err(NriError::CommandFailed(format!("{} {:?} -> {}: {}", program, args, code, err)))
        }
    }
}

pub fn default_runner(nsenter: &Option<Nsenter>) -> Runner {
    match nsenter {
        Some(ns) => Runner::Nsenter(ns.clone()),
        None => Runner::Local,
    }
}

