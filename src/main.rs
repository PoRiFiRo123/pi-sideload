//! pi-sideload: sign an .ipa with a free Apple ID and install it on an iPhone over Wi-Fi, with no screen.
//! Built on isideload (the library iloader uses) and idevice.
//!
//! Commands (all files live in --dir, default ~/.pi-sideload):
//!   login                 Ask for the Apple ID, password and 2FA code once; saves them for unattended runs.
//!   pair                  On a computer with the iPhone plugged in (USB): write pairing.plist and turn on Wi-Fi access.
//!   check  --host IP      Talk to the iPhone over Wi-Fi with pairing.plist (prints its name and iOS version).
//!   install APP.ipa --host IP [--usb] [--extensions register|reuse]
//!                         Sign and install. Unattended (no terminal) it never prompts: it fails with a clear message.

use std::io::{BufRead, IsTerminal, Write};
use std::net::IpAddr;
use std::path::{Path, PathBuf};

use idevice::{IdeviceService, RemoteXpcClient};
use idevice::core_device_proxy::CoreDeviceProxy;
use idevice::remote_pairing::{RemotePairingClient, RpPairingFile, RpPairingSocket, connect_tls_psk_tunnel_native};
use idevice::rsd::RsdHandshake;
use idevice::tcp::handle::AdapterHandle;
use idevice::lockdown::LockdownClient;
use idevice::pairing_file::PairingFile;
use idevice::provider::{IdeviceProvider, TcpProvider};
use idevice::usbmuxd::{Connection, UsbmuxdAddr, UsbmuxdConnection};
use isideload::anisette::remote_v3::RemoteV3AnisetteProvider;
use isideload::auth::apple_account::{AppleAccount, TwoFactorCallbackParams, TwoFactorCallbackResponse};
use isideload::dev::certificates::DevelopmentCertificate;
use isideload::dev::developer_session::DeveloperSession;
use isideload::sideload::SideloaderBuilder;
use isideload::sideload::builder::{ExtensionsBehavior, ExtensionsBehaviorSelection, MaxCertsBehavior};
use isideload::sideload::TeamSelection;
use isideload::util::callbacks::ExtensionsBehaviorCallbackBox;
use isideload::util::fs_storage::FsStorage;
use isideload::util::device::IdeviceInfo;
use isideload::dev::devices::DevicesApi;
use serde::{Deserialize, Serialize};

const LABEL: &str = "pi-sideload";

#[derive(Serialize, Deserialize)]
struct Account {
    apple_id: String,
    password: String,
}

type Res<T> = Result<T, String>;

fn fail<E: std::fmt::Display>(what: &str) -> impl FnOnce(E) -> String + '_ {
    move |e| format!("{what}: {e}")
}

struct Args {
    dir: PathBuf,
    cmd: String,
    positional: Vec<String>,
    host: Option<String>,
    usb: bool,
    extensions: String,
    out: Option<PathBuf>,
    rp_port: Option<u16>,
}

fn parse_args() -> Args {
    let home = std::env::var("HOME").unwrap_or_else(|_| ".".into());
    let mut a = Args {
        dir: PathBuf::from(home).join(".pi-sideload"),
        cmd: String::new(),
        positional: vec![],
        host: None,
        usb: false,
        extensions: "register".into(),
        out: None,
        rp_port: None,
    };
    let mut it = std::env::args().skip(1);
    while let Some(arg) = it.next() {
        match arg.as_str() {
            "--dir" => a.dir = PathBuf::from(it.next().unwrap_or_default()),
            "--host" => a.host = it.next(),
            "--usb" => a.usb = true,
            "--extensions" => a.extensions = it.next().unwrap_or_default(),
            "--out" => a.out = it.next().map(PathBuf::from),
            "--rp-port" => a.rp_port = it.next().and_then(|p| p.parse().ok()),
            "-h" | "--help" => a.cmd = "help".into(),
            _ if a.cmd.is_empty() => a.cmd = arg,
            _ => a.positional.push(arg),
        }
    }
    a
}

#[tokio::main]
async fn main() {
    rustls::crypto::ring::default_provider().install_default().ok();
    isideload::init().ok();
    let level = if std::env::var("PI_SIDELOAD_DEBUG").is_ok() { tracing::Level::DEBUG } else { tracing::Level::INFO };
    tracing_subscriber::fmt().with_max_level(level).with_target(false).init();

    let args = parse_args();
    let result = match args.cmd.as_str() {
        "login" => login(&args).await,
        "pair" => pair(&args).await,
        "check" => check(&args).await,
        "install" => install(&args).await,
        _ => {
            println!("{}", HELP);
            Ok(())
        }
    };
    if let Err(e) = result {
        eprintln!("error: {e}");
        std::process::exit(1);
    }
}

const HELP: &str = "pi-sideload: sign and install an .ipa over Wi-Fi with a free Apple ID

  pi-sideload login
  pi-sideload pair [--out pairing.plist]          (iPhone plugged in by USB, unlocked)
  pi-sideload check --host <iphone-ip> [--rp-port <port>]
  pi-sideload install <app.ipa> --host <iphone-ip> --rp-port <port> [--usb] [--extensions register|reuse]

Options: --dir <folder> (default ~/.pi-sideload). PI_SIDELOAD_DEBUG=1 for more logs.";

// ---------- files ----------

fn write_private(path: &Path, data: &[u8]) -> Res<()> {
    if let Some(p) = path.parent() {
        std::fs::create_dir_all(p).map_err(fail("create folder"))?;
    }
    std::fs::write(path, data).map_err(fail("write file"))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600)).ok();
    }
    Ok(())
}

fn read_account(dir: &Path) -> Res<Account> {
    let data = std::fs::read(dir.join("account.json"))
        .map_err(|_| "no saved Apple ID: run `pi-sideload login` first".to_string())?;
    serde_json::from_slice(&data).map_err(fail("account.json"))
}

fn prompt(q: &str) -> String {
    print!("{q}");
    std::io::stdout().flush().ok();
    let mut s = String::new();
    std::io::stdin().lock().read_line(&mut s).ok();
    s.trim().to_string()
}

fn prompt_hidden(q: &str) -> String {
    // stty is on every Raspberry Pi OS install; fall back to visible input if it isn't.
    let _ = std::process::Command::new("stty").arg("-echo").status();
    let s = prompt(q);
    let _ = std::process::Command::new("stty").arg("echo").status();
    println!();
    s
}

// ---------- Apple ID ----------

async fn sign_in(dir: &Path, acc: &Account) -> Res<AppleAccount> {
    let interactive = std::io::stdin().is_terminal();
    let anisette = RemoteV3AnisetteProvider::default()
        .map_err(fail("anisette"))?
        .set_storage(Box::new(FsStorage::new(dir.join("storage"))))
        .set_serial_number("2".to_string());

    let two_factor = move |p: TwoFactorCallbackParams| async move {
        if !interactive {
            eprintln!("Apple asked for a 2FA code. Run `pi-sideload login` in a terminal once to approve this Pi.");
            return Ok(TwoFactorCallbackResponse::Abort);
        }
        if let Some(e) = &p.last_error {
            println!("That didn't work: {e}");
        }
        let target = if p.sms {
            p.numbers
                .iter()
                .find(|n| Some(n.id) == p.selected_number_id)
                .map(|n| n.number_with_dial_code.clone())
                .unwrap_or_else(|| "your phone".into())
        } else {
            "your Apple devices".into()
        };
        println!("Enter the 2FA code sent to {target}.");
        if p.sms || p.unknown {
            println!("  (or \"d\" to send it to your devices instead)");
        }
        for n in p.numbers.iter().filter(|n| Some(n.id) != p.selected_number_id) {
            println!("  (or \"p{}\" to text {})", n.id, n.number_with_dial_code);
        }
        let code = prompt("Code: ");
        Ok(if code == "d" {
            TwoFactorCallbackResponse::SendToDevices
        } else if let Some(id) = code.strip_prefix('p').and_then(|x| x.parse().ok()) {
            TwoFactorCallbackResponse::SendSms(id)
        } else if code == "r" {
            TwoFactorCallbackResponse::ResendCode
        } else {
            TwoFactorCallbackResponse::SubmitCode(code)
        })
    };

    AppleAccount::builder(&acc.apple_id)
        .anisette_provider(anisette)
        .login(&acc.password, two_factor)
        .await
        .map_err(|e| format!("Apple ID sign-in failed: {e}"))
}

async fn login(a: &Args) -> Res<()> {
    let apple_id = std::env::var("PI_SIDELOAD_APPLE_ID").unwrap_or_else(|_| prompt("Apple ID (email): "));
    let password = std::env::var("PI_SIDELOAD_PASSWORD").unwrap_or_else(|_| prompt_hidden("Password: "));
    let acc = Account { apple_id, password };
    let mut account = sign_in(&a.dir, &acc).await?;
    DeveloperSession::from_account(&mut account)
        .await
        .map_err(|e| format!("signed in, but couldn't open the developer session: {e}"))?;
    write_private(&a.dir.join("account.json"), &serde_json::to_vec_pretty(&acc).unwrap())?;
    println!("Signed in. Saved to {} (readable only by you).", a.dir.join("account.json").display());
    Ok(())
}

// ---------- device ----------

fn pairing_path(a: &Args) -> PathBuf {
    a.out.clone().unwrap_or_else(|| a.dir.join("pairing.plist"))
}

async fn pair(a: &Args) -> Res<()> {
    let mut mux = UsbmuxdConnection::default().await.map_err(fail("usbmuxd (is it installed and running?)"))?;
    let devices = mux.get_devices().await.map_err(fail("list devices"))?;
    let dev = devices
        .iter()
        .find(|d| matches!(d.connection_type, Connection::Usb))
        .ok_or("no iPhone on USB: plug it in, unlock it and tap Trust")?;
    let mut pairing = mux.get_pair_record(&dev.udid).await.map_err(fail("read pair record (tap Trust on the iPhone)"))?;
    pairing.udid = Some(dev.udid.clone());

    // Let the phone answer lockdown over Wi-Fi with this pairing.
    let provider = dev.to_provider(UsbmuxdAddr::from_env_var().map_err(fail("usbmuxd address"))?, LABEL);
    let mut lockdown = LockdownClient::connect(&provider).await.map_err(fail("lockdown"))?;
    lockdown.start_session(&pairing).await.map_err(fail("lockdown session"))?;
    lockdown
        .set_value("EnableWifiDebugging", true.into(), Some("com.apple.mobile.wireless_lockdown"))
        .await
        .map_err(fail("turn on Wi-Fi access"))?;
    let name = lockdown.get_value(Some("DeviceName"), None).await.ok().and_then(|v| v.as_string().map(String::from));

    let out = pairing_path(a);
    write_private(&out, &pairing.serialize().map_err(fail("pairing file"))?)?;
    println!("Paired with {} ({}). Wrote {}", name.unwrap_or_else(|| "iPhone".into()), dev.udid, out.display());

    // iOS 17+ only lets apps be copied over Wi-Fi through a remote-pairing tunnel (what Xcode uses), so pair for that too.
    println!("Making the Wi-Fi tunnel pairing (tap Trust on the iPhone if it asks)…");
    let rp = rp_pair_usb(&provider).await?;
    let rp_out = out.with_file_name("rppairing.plist");
    write_private(&rp_out, &rp.to_bytes())?;
    println!("Wrote {}", rp_out.display());
    Ok(())
}

async fn rp_pair_usb(provider: &dyn IdeviceProvider) -> Res<RpPairingFile> {
    let proxy = CoreDeviceProxy::connect(provider).await.map_err(fail("CoreDeviceProxy (Developer Mode on?)"))?;
    let rsd_port = proxy.tunnel_info().server_rsd_port;
    let mut adapter = proxy.create_software_tunnel().map_err(fail("tunnel"))?.to_async_handle();
    let rsd = adapter.connect(rsd_port).await.map_err(fail("RSD connect"))?;
    let handshake = RsdHandshake::new(rsd).await.map_err(fail("RSD handshake"))?;
    let ts = handshake
        .services
        .get("com.apple.internal.dt.coredevice.untrusted.tunnelservice")
        .ok_or("the iPhone has no tunnel service")?;
    let stream = adapter.connect(ts.port).await.map_err(fail("tunnel service"))?;
    let mut conn = RemoteXpcClient::new(stream).await.map_err(fail("XPC"))?;
    conn.do_handshake().await.map_err(fail("XPC handshake"))?;
    let _ = conn.recv_root().await;
    let mut rpf = RpPairingFile::generate(LABEL);
    let mut rpc = RemotePairingClient::new(conn, LABEL);
    rpc.connect(&mut rpf, async || "000000".to_string()).await.map_err(|e| format!("remote pairing: {e:?}"))?;
    Ok(rpf)
}

/// Opens the Wi-Fi tunnel with rppairing.plist: remote pairing on `rp_port`, then TLS-PSK + CDTunnel, then RSD.
async fn wifi_tunnel(a: &Args) -> Res<(AdapterHandle, RsdHandshake)> {
    let host: IpAddr = a.host.as_deref().ok_or("--host is required")?.parse().map_err(|_| "bad --host")?;
    let port = a.rp_port.ok_or("--rp-port is required (the iPhone's _remotepairing._tcp port)")?;
    let path = pairing_path(a).with_file_name("rppairing.plist");
    let bytes = std::fs::read(&path).map_err(|_| format!("no {}: re-run `pi-sideload pair` with the iPhone on USB", path.display()))?;
    let mut rpf = RpPairingFile::from_bytes(&bytes).map_err(fail("rppairing.plist"))?;
    let addr = std::net::SocketAddr::new(host, port);
    let stream = tokio::net::TcpStream::connect(addr).await.map_err(fail("can't reach the iPhone's pairing port"))?;
    let mut rpc = RemotePairingClient::new(RpPairingSocket::new(stream), LABEL);
    rpc.connect(&mut rpf, async || "000000".to_string()).await.map_err(|e| format!("Wi-Fi pairing rejected: {e:?}"))?;
    let tunnel_port = rpc.create_tcp_listener().await.map_err(fail("tunnel listener"))?;
    let ts = tokio::net::TcpStream::connect(std::net::SocketAddr::new(host, tunnel_port)).await.map_err(fail("tunnel connect"))?;
    let tunnel = connect_tls_psk_tunnel_native(ts, rpc.encryption_key()).await.map_err(fail("tunnel TLS"))?;
    let client_ip: IpAddr = tunnel.info.client_address.parse().map_err(fail("tunnel address"))?;
    let server_ip: IpAddr = tunnel.info.server_address.parse().map_err(fail("tunnel address"))?;
    let mtu = tunnel.info.mtu as usize;
    let rsd_port = tunnel.info.server_rsd_port;
    let mut adapter = idevice::tcp::adapter::Adapter::new(Box::new(tunnel.into_inner()), client_ip, server_ip);
    adapter.set_mss(mtu.min(8192) - 60);
    let mut adapter = adapter.to_async_handle();
    let rsd = adapter.connect(rsd_port).await.map_err(fail("RSD connect"))?;
    let handshake = RsdHandshake::new(rsd).await.map_err(fail("RSD handshake"))?;
    Ok((adapter, handshake))
}

fn wifi_provider(a: &Args) -> Res<TcpProvider> {
    let host = a.host.as_deref().ok_or("--host <iphone-ip> is required")?;
    let addr: IpAddr = host.parse().map_err(|_| format!("not an IP address: {host}"))?;
    let path = pairing_path(a);
    let pairing_file = PairingFile::read_from_file(&path)
        .map_err(|e| format!("can't read {} ({e}): run `pi-sideload pair` with the iPhone on USB, then copy the file here", path.display()))?;
    Ok(TcpProvider { addr, scope_id: None, pairing_file, label: LABEL.into() })
}

async fn device_summary(p: &dyn IdeviceProvider, pairing: &PairingFile) -> Res<String> {
    let mut lockdown = LockdownClient::connect(p).await.map_err(fail("can't reach the iPhone (same Wi-Fi? unlocked recently?)"))?;
    lockdown.start_session(pairing).await.map_err(fail("pairing rejected (re-run `pi-sideload pair`)"))?;
    let get = |v: Option<plist::Value>| v.and_then(|v| v.as_string().map(String::from)).unwrap_or_default();
    let name = get(lockdown.get_value(Some("DeviceName"), None).await.ok());
    let ios = get(lockdown.get_value(Some("ProductVersion"), None).await.ok());
    Ok(format!("{name}, iOS {ios}"))
}

async fn check(a: &Args) -> Res<()> {
    let p = wifi_provider(a)?;
    println!("Reached {}", device_summary(&p, &p.pairing_file).await?);
    if a.rp_port.is_some() {
        let (_, hs) = wifi_tunnel(a).await?;
        println!("Wi-Fi tunnel works ({} services)", hs.services.len());
    }
    Ok(())
}

// ---------- install ----------

async fn install(a: &Args) -> Res<()> {
    let ipa = PathBuf::from(a.positional.first().ok_or("which .ipa? `pi-sideload install app.ipa --host <ip>`")?);
    if !ipa.exists() {
        return Err(format!("{} not found", ipa.display()));
    }
    let behavior = match a.extensions.as_str() {
        "reuse" => ExtensionsBehavior::ReuseMain,
        _ => ExtensionsBehavior::RegisterAll,
    };
    let acc = read_account(&a.dir)?;
    let mut account = sign_in(&a.dir, &acc).await?;
    let session = DeveloperSession::from_account(&mut account).await.map_err(fail("developer session"))?;

    let interactive = std::io::stdin().is_terminal();
    let max_certs = if interactive {
        MaxCertsBehavior::Prompt(Box::new(|certs: Vec<DevelopmentCertificate>| {
            Box::pin(async move {
                println!("A free Apple ID can have 2 signing certificates and both are used. Revoking one stops apps signed with it.");
                for (i, c) in certs.iter().enumerate() {
                    println!("  {}: {} on {}", i + 1, c.name.as_deref().unwrap_or("?"), c.machine_name.as_deref().unwrap_or("?"));
                }
                let pick = prompt("Number(s) to revoke, comma-separated (empty = cancel): ");
                let serials: Vec<String> = pick
                    .split(',')
                    .filter_map(|s| s.trim().parse::<usize>().ok())
                    .filter(|&n| n >= 1 && n <= certs.len())
                    .map(|n| certs[n - 1].serial_number.clone().unwrap_or_default())
                    .collect();
                Ok(if serials.is_empty() { None } else { Some(serials) })
            }) as isideload::util::callbacks::MaxCertsCallbackFuture
        }) as isideload::util::callbacks::MaxCertsCallbackBox)
    } else {
        MaxCertsBehavior::Error
    };

    let mut sideloader = SideloaderBuilder::<_, ExtensionsBehaviorCallbackBox>::new(session, acc.apple_id.clone())
        .team_selection(TeamSelection::First)
        .max_certs_behavior(max_certs)
        .extensions_behavior(ExtensionsBehaviorSelection::Fixed(behavior))
        .storage(Box::new(FsStorage::new(a.dir.join("storage"))))
        .machine_name("pi-sideload".to_string())
        .delete_app_after_install(true)
        .build();

    let progress = None::<fn(f32) -> std::future::Ready<()>>;
    if a.usb {
        let mut mux = UsbmuxdConnection::default().await.map_err(fail("usbmuxd"))?;
        let dev = mux.get_devices().await.map_err(fail("list devices"))?.into_iter().next().ok_or("no iPhone on USB")?;
        let p = dev.to_provider(UsbmuxdAddr::from_env_var().map_err(fail("usbmuxd address"))?, LABEL);
        sideloader.install_app(&p, ipa, false, progress, None).await.map_err(|e| format!("install failed: {e}"))?;
    } else {
        // Over Wi-Fi: sign once, then copy + install, retrying a dropped connection (the phone's Wi-Fi naps).
        let p = wifi_provider(a)?;
        println!("Installing on {}", device_summary(&p, &p.pairing_file).await?);
        let info = IdeviceInfo::from_device(&p).await.map_err(|e| format!("device info: {e}"))?;
        let team = sideloader.get_team().await.map_err(|e| format!("team: {e}"))?;
        sideloader
            .get_dev_session()
            .ensure_device_registered(&team, &info.name, &info.udid, None)
            .await
            .map_err(|e| format!("register device: {e}"))?;
        let (signed, _) = sideloader
            .sign_app(ipa, Some(team), false, progress, None, Some(&info.udid))
            .await
            .map_err(|e| format!("signing failed: {e}"))?;
        let mut last = String::new();
        for attempt in 1..=3u64 {
            println!("Copying to the iPhone over the Wi-Fi tunnel (try {attempt})…");
            let res = match wifi_tunnel(a).await {
                Ok((mut adapter, mut hs)) => isideload::sideload::install::install_app_rsd(&mut adapter, &mut hs, &signed, |pct| {
                    if pct % 20 == 0 { println!("  {pct}%"); }
                })
                .await
                .map_err(|e| format!("{e:?}")),
                Err(e) => Err(e),
            };
            match res {
                Ok(()) => { last.clear(); break; }
                Err(e) => {
                    eprintln!("try {attempt} failed: {e}");
                    last = e;
                    tokio::time::sleep(std::time::Duration::from_secs(5 * attempt)).await;
                }
            }
        }
        let _ = std::fs::remove_dir_all(&signed);
        if !last.is_empty() {
            return Err(format!("install failed after 4 tries: {last}"));
        }
    }
    println!("Installed.");
    Ok(())
}
