use std::{
    collections::HashSet,
    io::{BufRead, BufReader, Write},
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    sync::{Arc, Mutex},
};

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Target {
    pub exe: String,
    pub config: String,
    pub context: String,
}

impl Target {
    pub fn command(&self, args: &[String]) -> Command {
        let mut command = Command::new(&self.exe);
        if !self.config.is_empty() {
            command.args(["--kubeconfig", &self.config]);
        }
        if !self.context.is_empty() {
            command.args(["--context", &self.context]);
        }
        command.args(args);
        command
    }

    pub fn run(&self, args: &[String], input: Option<String>) -> Result<String, String> {
        const QUERY_TIMEOUT_MS: u64 = 30000;
        let mut scoped = vec![format!("--request-timeout={QUERY_TIMEOUT_MS}ms")];
        scoped.extend_from_slice(args);
        let mut child = self
            .command(&scoped)
            .stdin(if input.is_some() {
                Stdio::piped()
            } else {
                Stdio::null()
            })
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|e| format!("Cannot run {}: {e}", self.exe))?;
        let writer = input.map(|input| {
            let mut stdin = child.stdin.take().expect("piped stdin");
            std::thread::spawn(move || stdin.write_all(input.as_bytes()))
        });
        let output = child.wait_with_output();
        let written = writer.map(|writer| writer.join().expect("stdin writer"));
        let output = output.map_err(|e| e.to_string())?;
        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            let stdout = String::from_utf8_lossy(&output.stdout);
            return Err(format!("{}\n{}", stderr.trim(), stdout.trim())
                .trim()
                .to_owned());
        }
        if let Some(written) = written {
            written.map_err(|e| e.to_string())?;
        }
        Ok(String::from_utf8_lossy(&output.stdout).into_owned())
    }

    #[cfg(target_os = "macos")]
    pub fn open_terminal(&self, args: &[String]) -> Result<String, String> {
        let command = self.command(args);
        // AppleScript receives each argument independently and quotes it for the shell.
        let script = r#"
            on run argv
                set cmd to ""
                repeat with arg in argv
                    set cmd to cmd & " " & quoted form of (contents of arg)
                end repeat
                tell application "Terminal"
                    activate
                    do script cmd
                end tell
            end run
        "#;
        let output = Command::new("osascript")
            .args(["-e", script, "--"])
            .arg(command.get_program())
            .args(command.get_args())
            .output()
            .map_err(|e| e.to_string())?;
        if output.status.success() {
            Ok("Opened in Terminal".into())
        } else {
            Err(String::from_utf8_lossy(&output.stderr).trim().to_owned())
        }
    }
}

pub struct LocalContexts {
    pub entries: Vec<Target>,
    pub current: Option<Target>,
    pub error: Option<String>,
}

impl LocalContexts {
    pub fn discover(target: Target) -> Self {
        let home = std::env::var_os("HOME").map(PathBuf::from);
        let kube_dir = home.map(|home| home.join(".kube"));
        let mut loaded_paths = HashSet::new();
        if let Some(config) = std::env::var_os("KUBECONFIG").filter(|paths| !paths.is_empty()) {
            loaded_paths
                .extend(std::env::split_paths(&config).filter_map(|path| path.canonicalize().ok()));
        } else if let Some(path) = kube_dir
            .as_ref()
            .and_then(|dir| dir.join("config").canonicalize().ok())
        {
            loaded_paths.insert(path);
        }

        let mut result = Self {
            entries: Vec::new(),
            current: None,
            error: None,
        };
        match read_contexts(&target) {
            Ok((current, entries)) => {
                result.current = entries
                    .iter()
                    .find(|entry| entry.context == current)
                    .cloned();
                result.entries = entries;
            }
            Err(error) => result.error = Some(error),
        }
        if target.config.is_empty() {
            if let Some(dir) = kube_dir {
                result.scan_directory(&target.exe, &dir, &mut loaded_paths);
            }
        }
        result
    }

    fn scan_directory(&mut self, exe: &str, dir: &Path, loaded_paths: &mut HashSet<PathBuf>) {
        let Ok(files) = std::fs::read_dir(dir) else {
            return;
        };
        let mut paths: Vec<_> = files.flatten().map(|file| file.path()).collect();
        paths.sort();
        for path in paths {
            if !path.is_file()
                || !path
                    .canonicalize()
                    .is_ok_and(|path| loaded_paths.insert(path))
            {
                continue;
            }
            let Some(config) = path.to_str() else {
                continue;
            };
            let source = Target {
                exe: exe.into(),
                config: config.into(),
                context: String::new(),
            };
            if let Ok((_, entries)) = read_contexts(&source) {
                self.entries.extend(entries);
            }
        }
    }
}

fn read_contexts(source: &Target) -> Result<(String, Vec<Target>), String> {
    // Ask kubectl to resolve kubeconfig merging and return names only.
    let args = [
        "config",
        "view",
        "-o",
        r#"jsonpath={.current-context}{"\n"}{range .contexts[*]}{.name}{"\n"}{end}"#,
    ];
    let output = source.run(&args.map(str::to_owned), None)?;
    let mut lines = output.lines();
    let current = lines.next().unwrap_or_default().to_owned();
    let entries = lines
        .filter(|name| !name.is_empty())
        .map(|name| Target {
            context: name.into(),
            ..source.clone()
        })
        .collect();
    Ok((current, entries))
}

pub struct Stream {
    child: Child,
    output: Arc<Mutex<Vec<u8>>>,
    pub status: Option<String>,
}

impl Stream {
    pub fn start(target: &Target, args: &[String]) -> Result<Self, String> {
        let mut child = target
            .command(args)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|e| format!("Cannot run {}: {e}", target.exe))?;
        let output = Arc::new(Mutex::new(Vec::new()));
        Self::read(child.stdout.take().expect("piped stdout"), output.clone());
        Self::read(child.stderr.take().expect("piped stderr"), output.clone());
        Ok(Self {
            child,
            output,
            status: None,
        })
    }

    fn read(pipe: impl std::io::Read + Send + 'static, output: Arc<Mutex<Vec<u8>>>) {
        std::thread::spawn(move || {
            const MAX_BYTES: usize = 256 * 1024;
            let mut reader = BufReader::new(pipe);
            while let Ok(chunk) = reader.fill_buf() {
                if chunk.is_empty() {
                    break;
                }
                {
                    let mut output = output.lock().expect("stream buffer");
                    output.extend_from_slice(chunk);
                    let excess = output.len().saturating_sub(MAX_BYTES);
                    output.drain(..excess);
                }
                let len = chunk.len();
                reader.consume(len);
            }
        });
    }

    pub fn output(&self) -> String {
        String::from_utf8_lossy(&self.output.lock().expect("stream buffer")).into_owned()
    }

    pub fn poll(&mut self) {
        if self.status.is_none() {
            match self.child.try_wait() {
                Ok(Some(status)) => self.status = Some(format!("Session ended ({status})")),
                Err(error) => self.status = Some(error.to_string()),
                Ok(None) => {}
            }
        }
    }

    pub fn stop(&mut self) {
        if self.child.try_wait().ok().flatten().is_none() {
            let _ = self.child.kill();
            let _ = self.child.wait();
        }
        self.status = Some("Stopped".into());
    }
}

impl Drop for Stream {
    fn drop(&mut self) {
        self.stop();
    }
}
