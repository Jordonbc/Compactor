use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use crossbeam_channel::{bounded, Receiver};
use dirs_sys::known_folder;
use serde_derive::{Deserialize, Serialize};
use web_view::*;
use winapi::um::knownfolders;

use crate::backend::Backend;
use crate::config::Config;
use crate::folder::FolderSummary;
use crate::persistence::{self, config};

// messages received from the GUI
#[derive(Deserialize, Debug, Clone)]
#[serde(tag = "type")]
pub enum GuiRequest {
    OpenUrl {
        url: String,
    },
    SaveConfig {
        decimal: bool,
        compression: String,
        excludes: String,
    },
    ResetConfig,
    ChooseFolder,
    Compress,
    Decompress,
    Pause,
    Resume,
    Analyse,
    Stop,
    Quit,
}

// messages to send to the GUI
#[derive(Serialize)]
#[serde(tag = "type")]
pub enum GuiResponse {
    Version {
        date: String,
        version: String,
    },
    Config {
        decimal: bool,
        compression: String,
        excludes: String,
    },
    Folder {
        path: PathBuf,
    },
    Status {
        status: String,
        pct: Option<f32>,
    },
    FolderSummary {
        info: FolderSummary,
    },
    Paused,
    Resumed,
    Scanned,
    Stopped,
    Compacting,
}

pub struct GuiWrapper<T>(Handle<T>);

impl<T> GuiWrapper<T> {
    pub fn new(handle: Handle<T>) -> Self {
        let gui = Self(handle);
        gui.version();
        gui.config();
        gui
    }

    pub fn send(&self, msg: &GuiResponse) {
        let js = format!(
            "Response.dispatch(JSON.parse({}))",
            serde_json::to_string(msg)
                .and_then(|s| serde_json::to_string(&s))
                .expect("serialize")
        );
        self.0.dispatch(move |wv| wv.eval(&js)).ok(); // let errors bubble through via messages
    }

    pub fn version(&self) {
        let version = GuiResponse::Version {
            date: env!("VERGEN_BUILD_DATE").to_string(),
            version: format!("{}-{}", env!("CARGO_PKG_VERSION"), env!("VERGEN_SHA_SHORT")),
        };
        self.send(&version);
    }

    pub fn config(&self) {
        let s = config().read().unwrap().current();
        self.send(&GuiResponse::Config {
            decimal: s.decimal,
            compression: s.compression.to_string(),
            excludes: s.excludes.join("\n"),
        });
    }

    pub fn summary(&self, info: FolderSummary) {
        self.send(&GuiResponse::FolderSummary { info });
    }

    pub fn status<S: AsRef<str>>(&self, msg: S, val: Option<f32>) {
        self.send(&GuiResponse::Status {
            status: msg.as_ref().to_owned(),
            pct: val,
        });
    }

    pub fn folder<P: AsRef<Path>>(&self, path: P) {
        self.send(&GuiResponse::Folder {
            path: path.as_ref().to_path_buf(),
        });
    }

    pub fn paused(&self) {
        self.send(&GuiResponse::Paused);
    }

    pub fn resumed(&self) {
        self.send(&GuiResponse::Resumed);
    }

    pub fn scanned(&self) {
        self.send(&GuiResponse::Scanned);
    }

    pub fn stopped(&self) {
        self.send(&GuiResponse::Stopped);
    }

    pub fn compacting(&self) {
        self.send(&GuiResponse::Compacting);
    }

    pub fn choose_folder(&self) -> Receiver<Option<PathBuf>> {
        let (tx, rx) = bounded::<Option<PathBuf>>(1);
    
        let _ = self.0.dispatch(move |_| {
            // Get the directory where the application is running
            let folder = std::env::current_exe()
                .ok()
                .and_then(|path| path.parent().map(|p| p.to_path_buf()))
                .and_then(|path| path.to_str().map(str::to_string))
                .unwrap_or_default();
    
            let params = wfd::DialogParams {
                options: wfd::FOS_PICKFOLDERS,
                title: "Select a directory",
                default_folder: &folder,
                ..Default::default()
            };
    
            let _ = tx.send(
                wfd::open_dialog(params).map(|res| res.selected_file_path).ok()
            );
            Ok(())
        });
    
        rx
    }
}

pub fn spawn_gui() {
    let running = Arc::new(AtomicBool::new(true));
    let r = running.clone();
    ctrlc::set_handler(move || {
        r.store(false, Ordering::SeqCst);
    })
    .expect("Error setting Ctrl-C handler");

    let html = format!(
        include_str!("ui/index.html"),
        style = include_str!("ui/style.css"),
        script = format!(
            "{}\n{}",
            include_str!("ui/cash.min.js"),
            include_str!("ui/app.js")
        )
    );

    let (from_gui, from_gui_rx) = bounded::<GuiRequest>(128);

    let mut webview = web_view::builder()
        .title("Compactor")
        .content(Content::Html(html))
        .size(750, 430)
        .resizable(true)
        .debug(true)
        .user_data(())
        .invoke_handler(move |mut webview, arg| {
            match serde_json::from_str::<GuiRequest>(arg) {
                Ok(GuiRequest::OpenUrl { url }) => {
                    let _ = open::that(url);
                }
                Ok(GuiRequest::SaveConfig {
                    decimal,
                    compression,
                    excludes,
                }) => {
                    let s = Config {
                        decimal,
                        compression: compression.parse().unwrap_or_default(),
                        excludes: excludes.split('\n').map(str::to_owned).collect(),
                    };

                    if let Err(msg) = s.globset() {
                        tinyfiledialogs::message_box_ok(
                            "Settings Error",
                            &msg,
                            tinyfiledialogs::MessageBoxIcon::Error,
                        );
                    } else {
                        message_dispatch(
                            &mut webview,
                            &GuiResponse::Config {
                                decimal: s.decimal,
                                compression: s.compression.to_string(),
                                excludes: s.excludes.join("\n"),
                            },
                        );
                        let c = config();
                        let mut c = c.write().unwrap();
                        c.replace(s);
                        if let Err(e) = c.save() {
                            tinyfiledialogs::message_box_ok(
                                "Settings Error",
                                &format!("Error saving settings: {:?}", e),
                                tinyfiledialogs::MessageBoxIcon::Error,
                            );
                        }
                    }
                }
                Ok(GuiRequest::ResetConfig) => {
                    let s = Config::default();

                    message_dispatch(
                        &mut webview,
                        &GuiResponse::Config {
                            decimal: s.decimal,
                            compression: s.compression.to_string(),
                            excludes: s.excludes.join("\n"),
                        },
                    );
                    let c = config();
                    let mut c = c.write().unwrap();
                    c.replace(s);
                    if let Err(e) = c.save() {
                        tinyfiledialogs::message_box_ok(
                            "Settings Error",
                            &format!("Error saving settings: {:?}", e),
                            tinyfiledialogs::MessageBoxIcon::Error,
                        );
                    }
                }
                Ok(msg) => {
                    from_gui.send(msg).expect("GUI message queue");
                }
                Err(err) => {
                    eprintln!("Unhandled message {:?}: {:?}", arg, err);
                }
            }

            Ok(())
        })
        .build()
        .expect("WebView");

    persistence::init();

    let gui = GuiWrapper::new(webview.handle());
    let mut backend = Backend::new(gui, from_gui_rx);
    let bg = std::thread::spawn(move || {
        backend.run();
    });

    while running.load(Ordering::SeqCst) {
        match webview.step() {
            Some(Ok(_)) => (),
            Some(e) => {
                eprintln!("Error: {:?}", e);
            }
            None => {
                break;
            }
        }
    }

    webview.into_inner();

    bg.join().expect("background thread");
}

fn message_dispatch<T>(wv: &mut web_view::WebView<'_, T>, msg: &GuiResponse) {
    let js = format!(
        "Response.dispatch({})",
        serde_json::to_string(msg).expect("serialize")
    );

    wv.eval(&js).ok();
}
