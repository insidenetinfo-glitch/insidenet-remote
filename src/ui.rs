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

pub fn start(args: &mut [String]) {
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
        "data:image/png;base64,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MX0ub125Vl7f0IDZWSpId3/5h8Pvv/Pqts2bezr7+uno5H96rve9csBxG88PxyNGni1HfP6lly5pWHnKb6Kl1fVOs1l9aDSJvq4uRNuPJeJHDiLW08kkh4fYxKiTkcjlkGh0YFUaOvBNHIY4XXTEQXM2wscj25EqLruB/h6y9oqsRU35vZQsygV8tIWXjkVIdM6QrglrNGfY/OKMuWGmVD9jPipqqmDNtqLMO9gU3frpk0d2bt/5zttvd/zDe/iGZ3f/N+077YDjRqPHCX4gMbIbt6AwX7p40aKyuedf8jtnVuFZTkMuwkoxwv4k7HY72RuXifT38NzwILjREXAeF5PyE2eKUW2+DJdkyLFMohphao8LoI8tI2EykCszUoMho7TlMbLc/Iyksk5uKKtEUVkpyosKoOPCIxYuIQk27tvWt2Pz059/+P52u2P4b2crafPRVtp31PG+Vw7473JEYqTirK+uUlZOnV5lKSm15NfWN5hnL/pJj1htZPUy2JOAOw2QOZ8kR9T3U+CjETrglI6P7WZjCLxCdgnT/WEiKiiu0ethNOghZxkY5DKYM0mw7pEBidvR1PPFR797783XG2USCTNoH1Omp9fCiMeO+fFlNN8D+1454D/Mn5z4+leVZElJsaSspERbWlaWk1NTtzyl1ldasrLL1Tk2mTeV9ocTvIzRmxokUrHUqlYm5AwQS3KMNxCKsyIRZ5WJRvwD/Xt7WluaJTynH+5sP9jVdOSY3TmSGB11pdNUL+bv9n0mDnwvHfAfjETFE86IcYf8F45ApNZIhBpbZSCCLTublbCsKMHzGerMRKIjGhXSgiAkkkkhFv/3q3f/PhIpOqk7Ob6NNuGA/8LIMT3e2iK+mCFO8n8dnYg61diSbaKcMCZK9Xel0wkbswkH/B/af+21koj5T9/6mmNNONmETdiETdiETdiETdiETdiETdiETdiETdiETdiETdiETdiETdiETdiETdiETdiETdiETdiETdiETdiETdiETdiETdiETdiETdiETdiETdiETRi+Y/b/AJnuj+myNQEnAAAAAElFTkSuQmCC".into()
    }
    #[cfg(not(target_os = "macos"))] // 128x128 no padding
    {
        "data:image/png;base64,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".into()
    }
}
