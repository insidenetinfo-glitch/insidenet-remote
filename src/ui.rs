use std::{
    collections::HashMap,
    iter::FromIterator,
    sync::{Arc, Mutex},
};

use sciter::Value;

use hbb_common::{
    allow_err,
    config::{LocalConfig, PeerConfig},
    log,
};

#[cfg(not(feature = "flutter"))]
use crate::ui_session_interface::Session;
use crate::{common::get_app_name, ipc, ui_interface::*};

mod cm;
#[cfg(feature = "inline")]
pub mod inline;
pub mod remote;

#[allow(dead_code)]
type Status = (i32, bool, i64, String);

lazy_static::lazy_static! {
    // stupid workaround for https://sciter.com/forums/topic/crash-on-latest-tis-mac-sdk-sometimes/
    static ref STUPID_VALUES: Mutex<Vec<Arc<Vec<Value>>>> = Default::default();
}

#[cfg(not(feature = "flutter"))]
lazy_static::lazy_static! {
    pub static ref CUR_SESSION: Arc<Mutex<Option<Session<remote::SciterHandler>>>> = Default::default();
}

struct UIHostHandler;

pub fn start(args: &mut Vec<String>) {
    #[cfg(target_os = "macos")]
    crate::platform::delegate::show_dock();
    #[cfg(all(target_os = "linux", feature = "inline"))]
    {
        let app_dir = std::env::var("APPDIR").unwrap_or("".to_string());
        let mut so_path = "/usr/share/rustdesk/libsciter-gtk.so".to_owned();
        for (prefix, dir) in [
            ("", "/usr"),
            ("", "/app"),
            (&app_dir, "/usr"),
            (&app_dir, "/app"),
        ]
        .iter()
        {
            let path = format!("{prefix}{dir}/share/rustdesk/libsciter-gtk.so");
            if std::path::Path::new(&path).exists() {
                so_path = path;
                break;
            }
        }
        sciter::set_library(&so_path).ok();
    }
    #[cfg(windows)]
    // Check if there is a sciter.dll nearby.
    if let Ok(exe) = std::env::current_exe() {
        if let Some(parent) = exe.parent() {
            let sciter_dll_path = parent.join("sciter.dll");
            if sciter_dll_path.exists() {
                // Try to set the sciter dll.
                let p = sciter_dll_path.to_string_lossy().to_string();
                log::debug!("Found dll:{}, \n {:?}", p, sciter::set_library(&p));
            }
        }
    }
    // https://github.com/c-smile/sciter-sdk/blob/master/include/sciter-x-types.h
    // https://github.com/rustdesk/rustdesk/issues/132#issuecomment-886069737
    #[cfg(windows)]
    allow_err!(sciter::set_options(sciter::RuntimeOptions::GfxLayer(
        sciter::GFX_LAYER::WARP
    )));
    use sciter::SCRIPT_RUNTIME_FEATURES::*;
    allow_err!(sciter::set_options(sciter::RuntimeOptions::ScriptFeatures(
        ALLOW_FILE_IO as u8 | ALLOW_SOCKET_IO as u8 | ALLOW_EVAL as u8 | ALLOW_SYSINFO as u8
    )));
    let mut frame = sciter::WindowBuilder::main_window().create();
    #[cfg(windows)]
    allow_err!(sciter::set_options(sciter::RuntimeOptions::UxTheming(true)));
    frame.set_title(&crate::get_app_name());
    #[cfg(target_os = "macos")]
    crate::platform::delegate::make_menubar(frame.get_host(), args.is_empty());
    #[cfg(windows)]
    crate::platform::try_set_window_foreground(frame.get_hwnd() as _);
    #[cfg(windows)]
    crate::platform::windows::set_dark_title_bar(frame.get_hwnd() as _);
    let page;
    // Windows hands the URL scheme over as a single argument, e.g. `insideremote://44106022/`
    // or `insideremote://file-transfer/44106022`. Turn it into the usual `--connect <id>` form.
    // Query parameters (such as a password) are deliberately ignored.
    if args.len() == 1 && args[0].to_lowercase().starts_with(&crate::get_uri_prefix()) {
        let rest = &args[0][crate::get_uri_prefix().len()..];
        let rest = rest.split(|c| c == '?' || c == '#').next().unwrap_or("");
        let mut parts = rest.split('/').filter(|s| !s.is_empty());
        let first = parts.next().unwrap_or("");
        let (cmd, id) = match first {
            "connect" | "file-transfer" | "port-forward" | "rdp" => {
                (format!("--{}", first), parts.next().unwrap_or(""))
            }
            _ => ("--connect".to_owned(), first),
        };
        let id_ok = !id.is_empty()
            && id
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || "-_.@:".contains(c));
        if id_ok {
            *args = vec![cmd, id.to_owned()];
        }
    }
    if args.len() > 1 && args[0] == "--play" {
        args[0] = "--connect".to_owned();
        let path: std::path::PathBuf = (&args[1]).into();
        let id = path
            .file_stem()
            .map(|p| p.to_str().unwrap_or(""))
            .unwrap_or("")
            .to_owned();
        args[1] = id;
    }
    if args.is_empty() {
        std::thread::spawn(move || check_zombie());
        crate::common::check_software_update();
        frame.event_handler(UI {});
        frame.sciter_handler(UIHostHandler {});
        page = "index.html";
        // Start pulse audio local server.
        #[cfg(target_os = "linux")]
        std::thread::spawn(crate::ipc::start_pa);
    } else if args[0] == "--install" {
        frame.event_handler(UI {});
        frame.sciter_handler(UIHostHandler {});
        page = "install.html";
    } else if args[0] == "--cm" {
        frame.register_behavior("connection-manager", move || {
            Box::new(cm::SciterConnectionManager::new())
        });
        page = "cm.html";
        *cm::HIDE_CM.lock().unwrap() = crate::ipc::get_config("hide_cm")
            .ok()
            .flatten()
            .unwrap_or_default()
            == "true";
    } else if (args[0] == "--connect"
        || args[0] == "--file-transfer"
        || args[0] == "--port-forward"
        || args[0] == "--rdp")
        && args.len() > 1
    {
        #[cfg(windows)]
        {
            let hw = frame.get_host().get_hwnd();
            crate::platform::windows::enable_lowlevel_keyboard(hw as _);
        }
        let mut iter = args.iter();
        let Some(cmd) = iter.next() else {
            log::error!("Failed to get cmd arg");
            return;
        };
        let cmd = cmd.to_owned();
        let Some(id) = iter.next() else {
            log::error!("Failed to get id arg");
            return;
        };
        let id = id.to_owned();
        let pass = iter.next().unwrap_or(&"".to_owned()).clone();
        let args: Vec<String> = iter.map(|x| x.clone()).collect();
        frame.set_title(&id);
        frame.register_behavior("native-remote", move || {
            let handler =
                remote::SciterSession::new(cmd.clone(), id.clone(), pass.clone(), args.clone());
            #[cfg(not(feature = "flutter"))]
            {
                *CUR_SESSION.lock().unwrap() = Some(handler.inner());
            }
            Box::new(handler)
        });
        page = "remote.html";
    } else {
        log::error!("Wrong command: {:?}", args);
        return;
    }
    #[cfg(feature = "inline")]
    {
        let html = if page == "index.html" {
            inline::get_index()
        } else if page == "cm.html" {
            inline::get_cm()
        } else if page == "install.html" {
            inline::get_install()
        } else {
            inline::get_remote()
        };
        frame.load_html(html.as_bytes(), Some(page));
    }
    #[cfg(not(feature = "inline"))]
    frame.load_file(&format!(
        "file://{}/src/ui/{}",
        std::env::current_dir()
            .map(|c| c.display().to_string())
            .unwrap_or("".to_owned()),
        page
    ));
    let hide_cm = *cm::HIDE_CM.lock().unwrap();
    if !args.is_empty() && args[0] == "--cm" && hide_cm {
        // run_app calls expand(show) + run_loop, we use collapse(hide) + run_loop instead to create a hidden window
        frame.collapse(true);
        frame.run_loop();
        return;
    }
    frame.run_app();
}

struct UI {}

impl UI {
    fn recent_sessions_updated(&self) -> bool {
        recent_sessions_updated()
    }

    fn get_id(&self) -> String {
        ipc::get_id()
    }

    fn temporary_password(&mut self) -> String {
        temporary_password()
    }

    fn update_temporary_password(&self) {
        update_temporary_password()
    }

    fn set_permanent_password(&self, password: String) {
        let _ = set_permanent_password_with_result(password);
    }

    fn is_local_permanent_password_set(&self) -> bool {
        is_local_permanent_password_set()
    }

    fn is_permanent_password_set(&self) -> bool {
        is_permanent_password_set()
    }

    fn get_remote_id(&mut self) -> String {
        LocalConfig::get_remote_id()
    }

    fn set_remote_id(&mut self, id: String) {
        LocalConfig::set_remote_id(&id);
    }

    fn goto_install(&mut self) {
        goto_install();
    }

    fn install_me(&mut self, _options: String, _path: String) {
        install_me(_options, _path, false, false);
    }

    fn update_me(&self, _path: String) {
        update_me(_path);
    }

    fn run_without_install(&self) {
        run_without_install();
    }

    fn show_run_without_install(&self) -> bool {
        show_run_without_install()
    }

    fn get_license(&self) -> String {
        get_license()
    }

    fn get_option(&self, key: String) -> String {
        get_option(key)
    }

    fn get_local_option(&self, key: String) -> String {
        get_local_option(key)
    }

    fn set_local_option(&self, key: String, value: String) {
        set_local_option(key, value);
    }

    fn peer_has_password(&self, id: String) -> bool {
        peer_has_password(id)
    }

    fn forget_password(&self, id: String) {
        forget_password(id)
    }

    fn get_peer_option(&self, id: String, name: String) -> String {
        get_peer_option(id, name)
    }

    fn set_peer_option(&self, id: String, name: String, value: String) {
        set_peer_option(id, name, value)
    }

    fn using_public_server(&self) -> bool {
        crate::using_public_server()
    }

    fn is_incoming_only(&self) -> bool {
        hbb_common::config::is_incoming_only()
    }

    pub fn is_outgoing_only(&self) -> bool {
        hbb_common::config::is_outgoing_only()
    }

    pub fn is_custom_client(&self) -> bool {
        crate::common::is_custom_client()
    }

    pub fn is_disable_settings(&self) -> bool {
        hbb_common::config::is_disable_settings()
    }

    pub fn is_disable_account(&self) -> bool {
        hbb_common::config::is_disable_account()
    }

    pub fn is_disable_installation(&self) -> bool {
        hbb_common::config::is_disable_installation()
    }

    pub fn is_disable_ab(&self) -> bool {
        hbb_common::config::is_disable_ab()
    }

    fn get_options(&self) -> Value {
        let hashmap: HashMap<String, String> =
            serde_json::from_str(&get_options()).unwrap_or_default();
        let mut m = Value::map();
        for (k, v) in hashmap {
            m.set_item(k, v);
        }
        m
    }

    fn test_if_valid_server(&self, host: String, test_with_proxy: bool) -> String {
        test_if_valid_server(host, test_with_proxy)
    }

    fn get_sound_inputs(&self) -> Value {
        Value::from_iter(get_sound_inputs())
    }

    fn set_options(&self, v: Value) {
        let mut m = HashMap::new();
        for (k, v) in v.items() {
            if let Some(k) = k.as_string() {
                if let Some(v) = v.as_string() {
                    if !v.is_empty() {
                        m.insert(k, v);
                    }
                }
            }
        }
        set_options(m);
    }

    fn set_option(&self, key: String, value: String) {
        set_option(key, value);
    }

    fn install_path(&mut self) -> String {
        install_path()
    }

    fn install_options(&self) -> String {
        install_options()
    }

    fn get_socks(&self) -> Value {
        Value::from_iter(get_socks())
    }

    fn set_socks(&self, proxy: String, username: String, password: String) {
        set_socks(proxy, username, password)
    }

    fn is_installed(&self) -> bool {
        is_installed()
    }

    fn get_supported_privacy_mode_impls(&self) -> String {
        serde_json::to_string(&crate::privacy_mode::get_supported_privacy_mode_impl())
            .unwrap_or_default()
    }

    fn is_root(&self) -> bool {
        is_root()
    }

    fn is_release(&self) -> bool {
        #[cfg(not(debug_assertions))]
        return true;
        #[cfg(debug_assertions)]
        return false;
    }

    fn is_share_rdp(&self) -> bool {
        is_share_rdp()
    }

    fn set_share_rdp(&self, _enable: bool) {
        set_share_rdp(_enable);
    }

    fn is_installed_lower_version(&self) -> bool {
        is_installed_lower_version()
    }

    fn closing(&mut self, x: i32, y: i32, w: i32, h: i32) {
        crate::server::input_service::fix_key_down_timeout_at_exit();
        LocalConfig::set_size(x, y, w, h);
    }

    fn get_size(&mut self) -> Value {
        let s = LocalConfig::get_size();
        let mut v = Vec::new();
        v.push(s.0);
        v.push(s.1);
        v.push(s.2);
        v.push(s.3);
        Value::from_iter(v)
    }

    fn get_mouse_time(&self) -> f64 {
        get_mouse_time()
    }

    fn check_mouse_time(&self) {
        check_mouse_time()
    }

    fn get_connect_status(&mut self) -> Value {
        let mut v = Value::array(0);
        let x = get_connect_status();
        v.push(x.status_num);
        v.push(x.key_confirmed);
        v.push(x.id);
        v
    }

    #[inline]
    fn get_peer_value(id: String, p: PeerConfig) -> Value {
        let values = vec![
            id,
            p.info.username.clone(),
            p.info.hostname.clone(),
            p.info.platform.clone(),
            p.options.get("alias").unwrap_or(&"".to_owned()).to_owned(),
        ];
        Value::from_iter(values)
    }

    fn get_peer(&self, id: String) -> Value {
        let c = get_peer(id.clone());
        Self::get_peer_value(id, c)
    }

    fn get_fav(&self) -> Value {
        Value::from_iter(get_fav())
    }

    fn store_fav(&self, fav: Value) {
        let mut tmp = vec![];
        fav.values().for_each(|v| {
            if let Some(v) = v.as_string() {
                if !v.is_empty() {
                    tmp.push(v);
                }
            }
        });
        store_fav(tmp);
    }

    fn get_recent_sessions(&mut self) -> Value {
        // to-do: limit number of recent sessions, and remove old peer file
        let peers: Vec<Value> = PeerConfig::peers(None)
            .drain(..)
            .map(|p| Self::get_peer_value(p.0, p.2))
            .collect();
        static LAST_COUNT: std::sync::atomic::AtomicUsize =
            std::sync::atomic::AtomicUsize::new(usize::MAX);
        if LAST_COUNT.swap(peers.len(), std::sync::atomic::Ordering::Relaxed) != peers.len() {
            log::info!(
                "Recent sessions: {} (peers dir: {})",
                peers.len(),
                hbb_common::config::Config::path("peers").display()
            );
        }
        Value::from_iter(peers)
    }

    fn get_icon(&mut self) -> String {
        get_icon()
    }

    fn remove_peer(&mut self, id: String) {
        PeerConfig::remove(&id);
    }

    fn remove_discovered(&mut self, id: String) {
        remove_discovered(id);
    }

    fn send_wol(&mut self, id: String) {
        crate::lan::send_wol(id)
    }

    fn new_remote(&mut self, id: String, remote_type: String, force_relay: bool) {
        new_remote(id, remote_type, force_relay)
    }

    fn is_process_trusted(&mut self, _prompt: bool) -> bool {
        is_process_trusted(_prompt)
    }

    fn is_can_screen_recording(&mut self, _prompt: bool) -> bool {
        is_can_screen_recording(_prompt)
    }

    fn is_installed_daemon(&mut self, _prompt: bool) -> bool {
        is_installed_daemon(_prompt)
    }

    fn get_error(&mut self) -> String {
        get_error()
    }

    fn is_login_wayland(&mut self) -> bool {
        is_login_wayland()
    }

    fn current_is_wayland(&mut self) -> bool {
        current_is_wayland()
    }

    fn get_software_update_url(&self) -> String {
        crate::SOFTWARE_UPDATE_URL.lock().unwrap().clone()
    }

    fn get_new_version(&self) -> String {
        get_new_version()
    }

    fn get_version(&self) -> String {
        get_version()
    }

    fn get_fingerprint(&self) -> String {
        get_fingerprint()
    }

    fn get_app_name(&self) -> String {
        get_app_name()
    }

    fn get_software_ext(&self) -> String {
        #[cfg(windows)]
        let p = "exe";
        #[cfg(target_os = "macos")]
        let p = "dmg";
        #[cfg(target_os = "linux")]
        let p = "deb";
        p.to_owned()
    }

    fn get_software_store_path(&self) -> String {
        let mut p = std::env::temp_dir();
        let name = crate::SOFTWARE_UPDATE_URL
            .lock()
            .unwrap()
            .split("/")
            .last()
            .map(|x| x.to_owned())
            .unwrap_or(crate::get_app_name());
        p.push(name);
        format!("{}.{}", p.to_string_lossy(), self.get_software_ext())
    }

    fn create_shortcut(&self, _id: String) {
        #[cfg(windows)]
        create_shortcut(_id)
    }

    fn discover(&self) {
        std::thread::spawn(move || {
            allow_err!(crate::lan::discover());
        });
    }

    fn get_lan_peers(&self) -> String {
        // let peers = get_lan_peers()
        //     .into_iter()
        //     .map(|mut peer| {
        //         (
        //             peer.remove("id").unwrap_or_default(),
        //             peer.remove("username").unwrap_or_default(),
        //             peer.remove("hostname").unwrap_or_default(),
        //             peer.remove("platform").unwrap_or_default(),
        //         )
        //     })
        //     .collect::<Vec<(String, String, String, String)>>();
        serde_json::to_string(&get_lan_peers()).unwrap_or_default()
    }

    fn get_uuid(&self) -> String {
        get_uuid()
    }

    fn open_url(&self, url: String) {
        #[cfg(windows)]
        let p = "explorer";
        #[cfg(target_os = "macos")]
        let p = "open";
        #[cfg(target_os = "linux")]
        let p = if std::path::Path::new("/usr/bin/firefox").exists() {
            "firefox"
        } else {
            "xdg-open"
        };
        allow_err!(std::process::Command::new(p).arg(url).spawn());
    }

    fn change_id(&self, id: String) {
        reset_async_job_status();
        let old_id = self.get_id();
        change_id_shared(id, old_id);
    }

    fn http_request(&self, url: String, method: String, body: Option<String>, header: String) {
        http_request(url, method, body, header)
    }

    fn post_request(&self, url: String, body: String, header: String) {
        post_request(url, body, header)
    }

    fn is_ok_change_id(&self) -> bool {
        hbb_common::machine_uid::get().is_ok()
    }

    fn get_async_job_status(&self) -> String {
        get_async_job_status()
    }

    fn get_http_status(&self, url: String) -> Option<String> {
        get_async_http_status(url)
    }

    fn t(&self, name: String) -> String {
        crate::client::translate(name)
    }

    fn is_xfce(&self) -> bool {
        crate::platform::is_xfce()
    }

    fn get_api_server(&self) -> String {
        get_api_server()
    }

    fn has_hwcodec(&self) -> bool {
        has_hwcodec()
    }

    fn has_vram(&self) -> bool {
        has_vram()
    }

    fn get_langs(&self) -> String {
        get_langs()
    }

    fn video_save_directory(&self, root: bool) -> String {
        video_save_directory(root)
    }

    fn handle_relay_id(&self, id: String) -> String {
        handle_relay_id(&id).to_owned()
    }

    fn get_login_device_info(&self) -> String {
        get_login_device_info_json()
    }

    fn support_remove_wallpaper(&self) -> bool {
        support_remove_wallpaper()
    }

    fn has_valid_2fa(&self) -> bool {
        has_valid_2fa()
    }

    fn generate2fa(&self) -> String {
        generate2fa()
    }

    pub fn verify2fa(&self, code: String) -> bool {
        verify2fa(code)
    }

    fn verify_login(&self, raw: String, id: String) -> bool {
        crate::verify_login(&raw, &id)
    }

    fn generate_2fa_img_src(&self, data: String) -> String {
        let v = qrcode_generator::to_png_to_vec(data, qrcode_generator::QrCodeEcc::Low, 128)
            .unwrap_or_default();
        let s = hbb_common::sodiumoxide::base64::encode(
            v,
            hbb_common::sodiumoxide::base64::Variant::Original,
        );
        format!("data:image/png;base64,{s}")
    }

    pub fn check_hwcodec(&self) {
        check_hwcodec()
    }

    fn is_option_fixed(&self, key: String) -> bool {
        crate::ui_interface::is_option_fixed(&key)
    }

    fn get_builtin_option(&self, key: String) -> String {
        crate::ui_interface::get_builtin_option(&key)
    }

    fn is_remote_modify_enabled_by_control_permissions(&self) -> String {
        match crate::ui_interface::is_remote_modify_enabled_by_control_permissions() {
            Some(true) => "true",
            Some(false) => "false",
            None => "",
        }
        .to_string()
    }
}

impl sciter::EventHandler for UI {
    sciter::dispatch_script_call! {
        fn t(String);
        fn get_api_server();
        fn is_xfce();
        fn using_public_server();
        fn is_custom_client();
        fn is_outgoing_only();
        fn is_incoming_only();
        fn is_disable_settings();
        fn is_disable_account();
        fn is_disable_installation();
        fn is_disable_ab();
        fn get_id();
        fn temporary_password();
        fn update_temporary_password();
        fn set_permanent_password(String);
        fn is_local_permanent_password_set();
        fn is_permanent_password_set();
        fn get_remote_id();
        fn set_remote_id(String);
        fn closing(i32, i32, i32, i32);
        fn get_size();
        fn new_remote(String, String, bool);
        fn send_wol(String);
        fn remove_peer(String);
        fn remove_discovered(String);
        fn get_connect_status();
        fn get_mouse_time();
        fn check_mouse_time();
        fn get_recent_sessions();
        fn get_peer(String);
        fn get_fav();
        fn store_fav(Value);
        fn recent_sessions_updated();
        fn get_icon();
        fn install_me(String, String);
        fn is_installed();
        fn get_supported_privacy_mode_impls();
        fn is_root();
        fn is_release();
        fn set_socks(String, String, String);
        fn get_socks();
        fn is_share_rdp();
        fn set_share_rdp(bool);
        fn is_installed_lower_version();
        fn install_path();
        fn install_options();
        fn goto_install();
        fn is_process_trusted(bool);
        fn is_can_screen_recording(bool);
        fn is_installed_daemon(bool);
        fn get_error();
        fn is_login_wayland();
        fn current_is_wayland();
        fn get_options();
        fn get_option(String);
        fn get_local_option(String);
        fn set_local_option(String, String);
        fn get_peer_option(String, String);
        fn peer_has_password(String);
        fn forget_password(String);
        fn set_peer_option(String, String, String);
        fn get_license();
        fn test_if_valid_server(String, bool);
        fn get_sound_inputs();
        fn set_options(Value);
        fn set_option(String, String);
        fn get_software_update_url();
        fn get_new_version();
        fn get_version();
        fn get_fingerprint();
        fn update_me(String);
        fn show_run_without_install();
        fn run_without_install();
        fn get_app_name();
        fn get_software_store_path();
        fn get_software_ext();
        fn open_url(String);
        fn change_id(String);
        fn get_async_job_status();
        fn post_request(String, String, String);
        fn is_ok_change_id();
        fn create_shortcut(String);
        fn discover();
        fn get_lan_peers();
        fn get_uuid();
        fn has_hwcodec();
        fn has_vram();
        fn get_langs();
        fn video_save_directory(bool);
        fn handle_relay_id(String);
        fn get_login_device_info();
        fn support_remove_wallpaper();
        fn has_valid_2fa();
        fn generate2fa();
        fn generate_2fa_img_src(String);
        fn verify2fa(String);
        fn check_hwcodec();
        fn verify_login(String, String);
        fn is_option_fixed(String);
        fn get_builtin_option(String);
        fn is_remote_modify_enabled_by_control_permissions();
    }
}

impl sciter::host::HostHandler for UIHostHandler {
    fn on_graphics_critical_failure(&mut self) {
        log::error!("Critical rendering error: e.g. DirectX gfx driver error. Most probably bad gfx drivers.");
    }
}

#[cfg(not(target_os = "linux"))]
fn get_sound_inputs() -> Vec<String> {
    let mut out = Vec::new();
    use cpal::traits::{DeviceTrait, HostTrait};
    let host = cpal::default_host();
    if let Ok(devices) = host.devices() {
        for device in devices {
            if device.default_input_config().is_err() {
                continue;
            }
            if let Ok(name) = device.name() {
                out.push(name);
            }
        }
    }
    out
}

#[cfg(target_os = "linux")]
fn get_sound_inputs() -> Vec<String> {
    crate::platform::linux::get_pa_sources()
        .drain(..)
        .map(|x| x.1)
        .collect()
}

// sacrifice some memory
pub fn value_crash_workaround(values: &[Value]) -> Arc<Vec<Value>> {
    let persist = Arc::new(values.to_vec());
    STUPID_VALUES.lock().unwrap().push(persist.clone());
    persist
}

pub fn get_icon() -> String {
    // 128x128
    #[cfg(target_os = "macos")]
    // 128x128 on 160x160 canvas, then shrink to 128, mac looks better with padding
    {
        "data:image/png;base64,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".into()
    }
    #[cfg(not(target_os = "macos"))] // 128x128 no padding
    {
        "data:image/png;base64,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".into()
    }
}
