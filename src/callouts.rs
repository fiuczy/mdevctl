use anyhow::{Context, Result};
use log::{debug, warn};
use std::ffi::OsStr;
use std::fmt::{self, Display, Formatter};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};

use crate::mdev::*;

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Event {
    Pre,
    Post,
    Notify,
    Get,
}

#[derive(Debug)]
enum CalloutError {
    NoMatchingScript,
    InvocationFailure(PathBuf, Option<i32>),
    InvalidJSON(serde_json::Error),
}

impl Display for CalloutError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            CalloutError::NoMatchingScript => write!(f, "No matching script for device found"),
            CalloutError::InvocationFailure(p, i) => write!(
                f,
                "Script '{:?}' failed with status '{}'",
                p,
                match i {
                    Some(i) => i.to_string(),
                    None => "unknown".to_string(),
                }
            ),
            CalloutError::InvalidJSON(_) => {
                write!(f, "Invalid JSON received from callout script")
            }
        }
    }
}

impl std::error::Error for CalloutError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            CalloutError::InvalidJSON(e) => Some(e),
            _ => None,
        }
    }
}

impl Display for Event {
    fn fmt(&self, f: &mut Formatter) -> fmt::Result {
        match self {
            Event::Pre => write!(f, "pre"),
            Event::Post => write!(f, "post"),
            Event::Notify => write!(f, "notify"),
            Event::Get => write!(f, "get"),
        }
    }
}

#[derive(Clone, Copy, Debug)]
#[allow(dead_code)]
pub enum Action {
    Start,
    Stop,
    Define,
    Undefine,
    Modify,
    Attributes,
    Test, // used for tests only
}

impl Display for Action {
    fn fmt(&self, f: &mut Formatter) -> fmt::Result {
        match self {
            Action::Start => write!(f, "start"),
            Action::Stop => write!(f, "stop"),
            Action::Define => write!(f, "define"),
            Action::Undefine => write!(f, "undefine"),
            Action::Modify => write!(f, "modify"),
            Action::Attributes => write!(f, "attributes"),
            Action::Test => write!(f, "test"),
        }
    }
}

#[derive(Clone, Copy)]
pub enum State {
    None,
    Success,
    Failure,
}

impl Display for State {
    fn fmt(&self, f: &mut Formatter) -> fmt::Result {
        match self {
            State::None => write!(f, "none"),
            State::Success => write!(f, "success"),
            State::Failure => write!(f, "failure"),
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn invoke_callout_script(
    script: &Path,
    mdev_type: String,
    uuid: String,
    parent: String,
    event: Event,
    action: Action,
    state: State,
    stdin: String,
) -> Result<Output> {
    debug!(
        "{}-{}: executing {:?} (mdev_type={}, uuid={}, parent={}, state={})",
        event,
        action,
        script.as_os_str(),
        mdev_type,
        uuid,
        parent,
        state.to_string()
    );

    let mut cmd = Command::new(script.as_os_str());

    cmd.arg("-t")
        .arg(mdev_type)
        .arg("-e")
        .arg(event.to_string())
        .arg("-a")
        .arg(action.to_string())
        .arg("-s")
        .arg(state.to_string())
        .arg("-u")
        .arg(uuid)
        .arg("-p")
        .arg(parent)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());

    let mut child = cmd.spawn()?;
    if let Some(mut child_stdin) = child.stdin.take() {
        child_stdin
            .write_all(stdin.as_bytes())
            .context("Failed to write to stdin of command")?;
    }

    child.wait_with_output().map_err(anyhow::Error::from)
}

pub struct Callout {
    state: State,
    script: Option<PathBuf>,
}

impl Callout {
    fn new() -> Callout {
        Callout {
            state: State::None,
            script: None,
        }
    }

    pub fn invoke<F>(dev: &mut MDev, action: Action, force: bool, func: F) -> Result<()>
    where
        F: Fn(&mut MDev) -> Result<()>,
    {
        let mut c = Callout::new();

        let res = c
            .callout(dev, Event::Pre, action)
            .or_else(|e| {
                force
                    .then(|| {
                        warn!(
                            "Forcing operation '{}' despite callout failure. Error was: {}",
                            action, e
                        );
                    })
                    .ok_or(e)
            })
            .and_then(|_| {
                let tmp_res = func(dev);
                c.state = match tmp_res {
                    Ok(_) => State::Success,
                    Err(_) => State::Failure,
                };

                let post_res = c.callout(dev, Event::Post, action);
                if post_res.is_err() {
                    debug!("Error occurred when executing post callout script");
                }

                tmp_res
            });

        c.notify(dev, action);
        res
    }

    fn parse_attribute_output(
        &self,
        dev: &mut MDev,
        path: &PathBuf,
        output: Output,
    ) -> Result<serde_json::Value, CalloutError> {
        if output.status.success() {
            debug!("Get attributes successfully from callout script");
            let mut st = String::from_utf8_lossy(&output.stdout).to_string();

            if st.is_empty() {
                return Ok(serde_json::Value::Null);
            }

            if &st == "[{}]" {
                debug!(
                    "Attribute field for {} is empty",
                    dev.uuid.hyphenated().to_string()
                );
                st = "[]".to_string();
            }

            serde_json::from_str(&st).map_err(CalloutError::InvalidJSON)
        } else {
            self.print_err(&output, path);
            Err(CalloutError::InvocationFailure(
                path.clone(),
                output.status.code(),
            ))
        }
    }

    fn get_attributes_dir(dev: &mut MDev, dir: PathBuf) -> Result<serde_json::Value, CalloutError> {
        let event = Event::Get;
        let action = Action::Attributes;
        let c = Callout::new();

        match c.invoke_first_matching_script(dev, dir, event, action) {
            Some((path, output)) => c.parse_attribute_output(dev, &path, output),
            None => {
                debug!(
                    "Device type {} unmatched by callout script",
                    dev.mdev_type.as_ref().unwrap()
                );
                Err(CalloutError::NoMatchingScript)
            }
        }
    }

    pub fn get_attributes(dev: &mut MDev) -> Result<serde_json::Value> {
        for dir in dev.env.callout_dirs() {
            if dir.is_dir() {
                let res = Self::get_attributes_dir(dev, dir);
                if let Err(CalloutError::NoMatchingScript) = res {
                    continue;
                }

                return res.map_err(anyhow::Error::from);
            }
        }
        Ok(serde_json::Value::Null)
    }

    fn invoke_script<P: AsRef<Path>>(
        &self,
        dev: &mut MDev,
        script: P,
        event: Event,
        action: Action,
    ) -> Result<Output> {
        debug!(
            "{}-{}: executing {:?}",
            event,
            action,
            script.as_ref().as_os_str()
        );

        let stdin = match event {
            Event::Get => String::new(),
            _ => dev.to_json(false)?.to_string(),
        };

        invoke_callout_script(
            script.as_ref(),
            dev.mdev_type().unwrap().to_string(),
            dev.uuid.to_string(),
            dev.parent().unwrap().to_string(),
            event,
            action,
            self.state,
            stdin,
        )
    }

    fn print_err<P: AsRef<Path>>(&self, output: &Output, script: P) {
        let sname = script
            .as_ref()
            .file_name()
            .unwrap_or_else(|| OsStr::new("unknown script name"))
            .to_string_lossy();

        let st = String::from_utf8_lossy(&output.stderr);
        if !st.is_empty() {
            eprint!("{}: {}", &sname, st);
        }
    }

    fn invoke_first_matching_script<P: AsRef<Path> + std::fmt::Debug>(
        &self,
        dev: &mut MDev,
        dir: P,
        event: Event,
        action: Action,
    ) -> Option<(PathBuf, Output)> {
        debug!(
            "{}-{}: looking for a matching callout script for dev type '{}' in {:?}",
            event,
            action,
            dev.mdev_type.as_ref()?,
            dir
        );

        let mut sorted_paths = dir
            .as_ref()
            .read_dir()
            .ok()?
            .filter_map(|k| k.ok().map(|e| e.path()))
            .collect::<Vec<_>>();

        sorted_paths.sort();

        for path in sorted_paths {
            match self.invoke_script(dev, &path, event, action) {
                Ok(res) => {
                    if res.status.code().is_none() {
                        warn!("callout script {:?} was terminated by a signal", path);
                        continue;
                    } else if res.status.code() != Some(2) {
                        debug!("found callout script {:?}", path);
                        return Some((path, res));
                    } else {
                        debug!(
                            "device type {} unmatched by callout script",
                            dev.mdev_type().ok()?
                        );
                    }
                }
                Err(e) => {
                    debug!("failed to execute callout script {:?}: {:?}", path, e);
                    continue;
                }
            }
        }
        None
    }

    fn callout_dir(
        &mut self,
        dev: &mut MDev,
        event: Event,
        action: Action,
        dir: PathBuf,
    ) -> Result<(), CalloutError> {
        let rc = match self.script {
            Some(ref s) => self
                .invoke_script(dev, s, event, action)
                .ok()
                .and_then(|output| {
                    self.print_err(&output, s);
                    output.status.code()
                }),
            _ => {
                if !dir.is_dir() {
                    return Err(CalloutError::NoMatchingScript);
                }
                self.invoke_first_matching_script(dev, dir, event, action)
                    .and_then(|(path, output)| {
                        self.print_err(&output, &path);
                        self.script = Some(path);
                        output.status.code()
                    })
            }
        };

        match rc {
            Some(0) => Ok(()),
            Some(n) => Err(CalloutError::InvocationFailure(
                self.script.as_ref().unwrap().to_path_buf(),
                Some(n),
            )),
            None => Err(CalloutError::NoMatchingScript),
        }
    }

    fn callout(&mut self, dev: &mut MDev, event: Event, action: Action) -> Result<()> {
        for dir in dev.env.callout_dirs() {
            let res = self.callout_dir(dev, event, action, dir);

            if let Err(CalloutError::NoMatchingScript) = res {
                continue;
            }

            return res.map_err(anyhow::Error::from);
        }
        Ok(())
    }

    fn notify(&mut self, dev: &mut MDev, action: Action) {
        let event = Event::Notify;
        let dirs = dev.env.notification_dirs();
        debug!(
            "{}-{}: executing notification scripts for device {}",
            event, action, dev.uuid
        );

        for dir in dirs {
            if !dir.is_dir() {
                continue;
            }

            if let Ok(readdir) = dir.read_dir() {
                for path in readdir.filter_map(|x| x.ok().map(|y| y.path())) {
                    match self.invoke_script(dev, &path, event, action) {
                        Ok(output) => {
                            if !output.status.success() {
                                debug!("Error occurred when executing notify script {:?}", path);
                            }
                        }
                        _ => {
                            debug!("Failed to execute callout script {:?}", path);
                            continue;
                        }
                    }
                }
            }
        }
    }
}
