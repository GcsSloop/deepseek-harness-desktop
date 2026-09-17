use std::{
    io::{BufRead, BufReader, Read},
    net::{Ipv4Addr, SocketAddrV4, TcpListener, TcpStream},
    path::PathBuf,
    process::{Child, Command, Stdio},
    sync::{Arc, Mutex},
    thread,
    time::{Duration, Instant},
};

use tauri::Manager;

mod control_server;
mod native_browser;
mod panel_bridge;
mod window_guard;

use native_browser::NativeBrowser;

struct HarnessProcess(Arc<Mutex<Option<Child>>>);

impl HarnessProcess {
    fn stop(&self) {
        let mut process = self.0.lock().expect("harness process lock poisoned");
        if let Some(mut child) = process.take() {
            #[cfg(unix)]
            unsafe {
                libc::kill(-(child.id() as i32), libc::SIGTERM);
            }

            #[cfg(windows)]
            {
                let _ = Command::new("taskkill")
                    .args(["/PID", &child.id().to_string(), "/T", "/F"])
                    .stdout(Stdio::null())
                    .stderr(Stdio::null())
                    .status();
            }

            let deadline = Instant::now() + Duration::from_secs(3);
            while Instant::now() < deadline {
                if child.try_wait().ok().flatten().is_some() {
                    return;
                }
                thread::sleep(Duration::from_millis(50));
            }
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

impl Drop for HarnessProcess {
    fn drop(&mut self) {
        self.stop();
    }
}

fn free_port() -> Result<u16, String> {
    TcpListener::bind(SocketAddrV4::new(Ipv4Addr::LOCALHOST, 0))
        .and_then(|listener| listener.local_addr())
        .map(|address| address.port())
        .map_err(|error| format!("无法分配本地端口：{error}"))
}

fn runtime_paths(resource_dir: PathBuf) -> (PathBuf, PathBuf) {
    let harness = resource_dir.join("resources").join("harness");
    let node = if cfg!(windows) {
        resource_dir.join("resources").join("node").join("node.exe")
    } else {
        resource_dir
            .join("resources")
            .join("node")
            .join("bin")
            .join("node")
    };
    (node, harness)
}

fn start_harness(
    app: &tauri::AppHandle,
) -> Result<(HarnessProcess, u16, Arc<Mutex<Option<String>>>), String> {
    let resource_dir = app
        .path()
        .resource_dir()
        .map_err(|error| format!("无法定位应用资源：{error}"))?;
    let (node, harness) = runtime_paths(resource_dir);
    let entry = harness
        .join("node_modules")
        .join("@deepseek-ai")
        .join("dsh")
        .join("lib")
        .join("bin.js");

    if !node.is_file() || !entry.is_file() {
        return Err("Harness 运行资源不完整，请重新安装应用。".into());
    }

    let port = free_port()?;
    let mut command = Command::new(node);
    command
        .arg(entry)
        .args([
            "web",
            "--no-open",
            "--host",
            "127.0.0.1",
            "--port",
            &port.to_string(),
        ])
        .current_dir(&harness)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());

    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        unsafe {
            command.pre_exec(|| {
                if libc::setsid() == -1 {
                    return Err(std::io::Error::last_os_error());
                }
                Ok(())
            });
        }
    }

    let mut child = command
        .spawn()
        .map_err(|error| format!("无法启动 Harness：{error}"))?;
    let authenticated_url = Arc::new(Mutex::new(None));
    fn capture_url<R: Read + Send + 'static>(stream: R, target: Arc<Mutex<Option<String>>>) {
        thread::spawn(move || {
            for line in BufReader::new(stream).lines().map_while(Result::ok) {
                let Some(start) = line.find("dsh web: ") else {
                    continue;
                };
                let Some(url) = line[start + "dsh web: ".len()..].split_whitespace().next() else {
                    continue;
                };
                if let Ok(mut target) = target.lock() {
                    *target = Some(url.to_owned());
                }
                break;
            }
        });
    }
    if let Some(stdout) = child.stdout.take() {
        capture_url(stdout, Arc::clone(&authenticated_url));
    }
    if let Some(stderr) = child.stderr.take() {
        capture_url(stderr, Arc::clone(&authenticated_url));
    }
    Ok((
        HarnessProcess(Arc::new(Mutex::new(Some(child)))),
        port,
        authenticated_url,
    ))
}

pub fn run() {
    let app = tauri::Builder::default()
        .setup(|app| {
            let (process, port, authenticated_url) = start_harness(app.handle())?;
            app.manage(process);

            // The native browser panel is published on a loopback control API;
            // a web UI that wants it discovers the descriptor under $DSH_HOME.
            // Nothing here depends on that UI, and the harness is untouched.
            app.manage(NativeBrowser::default());
            // Escape must not leave fullscreen; the guard only consumes it there.
            if let Some(window) = app.get_window("main") {
                window_guard::install(window);
            }
            match control_server::start(app.handle().clone()) {
                Ok(control_port) => {
                    if let Err(error) = control_server::advertise(control_port) {
                        eprintln!("native browser panel: {error}");
                    }
                }
                Err(error) => eprintln!("native browser panel: {error}"),
            }

            let handle = app.handle().clone();
            thread::spawn(move || {
                let address = SocketAddrV4::new(Ipv4Addr::LOCALHOST, port);
                let deadline = Instant::now() + Duration::from_secs(45);
                while Instant::now() < deadline {
                    if TcpStream::connect_timeout(&address.into(), Duration::from_millis(250)).is_ok() {
                        let url = authenticated_url
                            .lock()
                            .ok()
                            .and_then(|target| target.clone());
                        if let Some(url) = url {
                            let url = match url.parse() {
                                Ok(url) => url,
                                Err(_) => return,
                            };
                            let main_handle = handle.clone();
                            let _ = handle.run_on_main_thread(move || {
                                if let Some(window) = main_handle.get_webview_window("main") {
                                    let _ = window.navigate(url);
                                }
                            });
                            return;
                        }
                    }
                    thread::sleep(Duration::from_millis(150));
                }
            });
            Ok(())
        })
        .on_window_event(|window, event| {
            if matches!(event, tauri::WindowEvent::CloseRequested { .. }) {
                window.state::<HarnessProcess>().stop();
            }
        })
        .build(tauri::generate_context!())
        .expect("error while building DeepSeek Harness Desktop");

    app.run(|app_handle, event| {
        if matches!(
            event,
            tauri::RunEvent::Exit | tauri::RunEvent::ExitRequested { .. }
        ) {
            app_handle.state::<HarnessProcess>().stop();
            control_server::withdraw();
        }
    });
}
