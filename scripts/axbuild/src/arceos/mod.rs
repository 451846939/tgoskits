use std::{
    io::{Read, Write},
    net::TcpStream,
    path::{Path, PathBuf},
    sync::{Arc, atomic::AtomicBool},
    thread,
    time::{Duration, Instant},
};

use anyhow::{Context, anyhow, bail};
use clap::{Args, Subcommand};
use ostool::{
    board::{RunBoardOptions, config::BoardRunConfig},
    build::config::Cargo,
};

use crate::{
    axvisor::test::host_probe::{HostTcpProbeFn, HostTcpProbeGuard},
    context::{
        AppContext, BuildCliArgs, ResolvedBuildRequest, SnapshotPersistence,
        resolve_arceos_arch_and_target,
    },
    test::host_http::HostHttpServerGuard,
};

const DEFAULT_AICP_HOST_PROBE_PORT: u16 = 18_800;
const DEFAULT_AICP_HOST_PROBE_CONNECT_TIMEOUT_SECS: u64 = 120;
const AICP_HOST_PROBE_IO_TIMEOUT: Duration = Duration::from_secs(3);
const AICP_HOST_PROBE_RESPONSE_TIMEOUT: Duration = Duration::from_secs(30);
const AICP_HOST_PROBE_RETRY_INTERVAL: Duration = Duration::from_millis(100);

/// Host-side AICP handshake probe for an ArceOS QEMU application.
///
/// The QEMU config owns whether this is enabled. Keeping the transport check in
/// axbuild means the standalone application command verifies the same hostfwd
/// path that users and CI invoke, rather than relying on a serial ready marker.
#[derive(Clone, Debug, serde::Deserialize)]
struct AicpHostProbeConfig {
    #[serde(default = "default_aicp_host_probe_port")]
    host_port: u16,
    #[serde(default = "default_aicp_host_probe_connect_timeout_secs")]
    connect_timeout_secs: u64,
}

fn default_aicp_host_probe_port() -> u16 {
    DEFAULT_AICP_HOST_PROBE_PORT
}

fn default_aicp_host_probe_connect_timeout_secs() -> u64 {
    DEFAULT_AICP_HOST_PROBE_CONNECT_TIMEOUT_SECS
}

fn load_aicp_host_probe_config(
    qemu_config_path: Option<&Path>,
) -> anyhow::Result<Option<AicpHostProbeConfig>> {
    #[derive(serde::Deserialize)]
    struct ProbeSection {
        #[serde(default)]
        host_aicp_probe: Option<AicpHostProbeConfig>,
    }

    let Some(path) = qemu_config_path else {
        return Ok(None);
    };
    let contents = std::fs::read_to_string(path)
        .with_context(|| format!("failed to read {}", path.display()))?;
    Ok(toml::from_str::<ProbeSection>(&contents)
        .with_context(|| format!("failed to parse {}", path.display()))?
        .host_aicp_probe)
}

fn start_qemu_aicp_host_probe(
    request: &ResolvedBuildRequest,
    qemu: &mut ostool::run::qemu::QemuConfig,
) -> anyhow::Result<Option<HostTcpProbeGuard>> {
    let Some(config) = load_aicp_host_probe_config(request.qemu_config.as_deref())? else {
        return Ok(None);
    };

    let qmp_socket = std::env::temp_dir().join(format!(
        "arceos-aicp-qmp-{}-{}.sock",
        request.package,
        std::process::id()
    ));
    qemu.args.extend([
        "-qmp".to_string(),
        format!("unix:{},server=on,wait=off", qmp_socket.to_string_lossy()),
    ]);
    let host_port = config.host_port;
    let probe: HostTcpProbeFn = Box::new(move || run_aicp_hello_probe(host_port));
    let stop = Arc::new(AtomicBool::new(false));
    HostTcpProbeGuard::start(
        host_port,
        8800,
        config.connect_timeout_secs,
        &request.package,
        Some(qmp_socket),
        stop,
        probe,
    )
    .map(Some)
}

fn run_aicp_hello_probe(host_port: u16) -> anyhow::Result<()> {
    let started = Instant::now();
    loop {
        match run_aicp_hello_probe_attempt(host_port) {
            Ok(()) => return Ok(()),
            Err(error) => {
                if started.elapsed() >= AICP_HOST_PROBE_RESPONSE_TIMEOUT {
                    return Err(error).context(
                        "AICP server accepted TCP but did not complete a HELLO/STATUS exchange",
                    );
                }
                thread::sleep(AICP_HOST_PROBE_RETRY_INTERVAL);
            }
        }
    }
}

fn run_aicp_hello_probe_attempt(host_port: u16) -> anyhow::Result<()> {
    const HEADER_LEN: usize = 32;
    const STATUS_PAYLOAD_LEN: usize = 24;
    const MAGIC: u16 = 0xA1C0;
    const VERSION: u8 = 1;
    const MSG_HELLO: u8 = 1;
    const MSG_STATUS: u8 = 3;
    const ERROR_OK: u16 = 0;

    let mut stream = TcpStream::connect(("127.0.0.1", host_port))
        .with_context(|| format!("failed to connect AICP hostfwd at 127.0.0.1:{host_port}"))?;
    stream
        .set_read_timeout(Some(AICP_HOST_PROBE_IO_TIMEOUT))
        .context("failed to set AICP probe read timeout")?;
    stream
        .set_write_timeout(Some(AICP_HOST_PROBE_IO_TIMEOUT))
        .context("failed to set AICP probe write timeout")?;

    let mut request = [0_u8; HEADER_LEN];
    request[0..2].copy_from_slice(&MAGIC.to_be_bytes());
    request[2] = VERSION;
    request[3] = MSG_HELLO;
    request[6..8].copy_from_slice(&(HEADER_LEN as u16).to_be_bytes());
    request[12..16].copy_from_slice(&1_u32.to_be_bytes());
    request[24..26].copy_from_slice(&ERROR_OK.to_be_bytes());
    let request_crc = aicp_frame_crc(&request, &[]);
    request[26..28].copy_from_slice(&request_crc.to_be_bytes());
    stream
        .write_all(&request)
        .context("failed to send AICP HELLO")?;

    let mut header_wire = [0_u8; HEADER_LEN];
    stream
        .read_exact(&mut header_wire)
        .context("failed to read AICP STATUS header")?;
    let magic = u16::from_be_bytes(header_wire[0..2].try_into().unwrap());
    let version = header_wire[2];
    let message_type = header_wire[3];
    let header_len = u16::from_be_bytes(header_wire[6..8].try_into().unwrap()) as usize;
    let payload_len = u32::from_be_bytes(header_wire[8..12].try_into().unwrap()) as usize;
    let sequence = u32::from_be_bytes(header_wire[12..16].try_into().unwrap());
    let error_code = u16::from_be_bytes(header_wire[24..26].try_into().unwrap());
    let crc16 = u16::from_be_bytes(header_wire[26..28].try_into().unwrap());
    if magic != MAGIC || version != VERSION || header_len != HEADER_LEN {
        bail!("invalid AICP HELLO response header");
    }
    if payload_len != STATUS_PAYLOAD_LEN {
        bail!(
            "AICP HELLO response has payload length {payload_len}, expected {STATUS_PAYLOAD_LEN}"
        );
    }
    let mut payload = [0_u8; STATUS_PAYLOAD_LEN];
    stream
        .read_exact(&mut payload)
        .context("failed to read AICP STATUS payload")?;
    if aicp_frame_crc(&header_wire, &payload) != crc16 {
        bail!("AICP HELLO response has an invalid CRC");
    }
    if message_type != MSG_STATUS || sequence != 1 || error_code != ERROR_OK {
        bail!(
            "unexpected AICP HELLO response: type={} seq={} error={}",
            message_type,
            sequence,
            error_code
        );
    }
    let mode = u32::from_be_bytes(payload[16..20].try_into().unwrap());
    let applied_seq = u32::from_be_bytes(payload[20..24].try_into().unwrap());
    println!(
        "AICP_HOST_PROBE_PASSED seq={} mode={} applied_seq={}",
        sequence, mode, applied_seq
    );
    Ok(())
}

/// CRC-16/CCITT-FALSE for the fixed AICP frame layout. The host probe is a
/// consumer-side integration check, so it keeps only the wire operations it
/// must verify and deliberately does not expose another protocol API from
/// axbuild.
fn aicp_frame_crc(header: &[u8; 32], payload: &[u8]) -> u16 {
    let mut wire = *header;
    wire[26] = 0;
    wire[27] = 0;
    let mut crc = 0xffff_u16;
    for byte in wire.into_iter().chain(payload.iter().copied()) {
        crc ^= (byte as u16) << 8;
        for _ in 0..8 {
            crc = if crc & 0x8000 != 0 {
                (crc << 1) ^ 0x1021
            } else {
                crc << 1
            };
        }
    }
    crc
}

mod board;
pub mod build;
pub mod cbuild;
pub mod config;
pub mod rootfs;
pub mod test;

fn start_qemu_host_http_server(
    request: &ResolvedBuildRequest,
) -> anyhow::Result<Option<HostHttpServerGuard>> {
    request
        .qemu_config
        .as_deref()
        .map(crate::test::qemu::load_qemu_case_host_http_server)
        .transpose()?
        .flatten()
        .map(|config| HostHttpServerGuard::start(&config, &request.package))
        .transpose()
}

// ---------------------------------------------------------------------------
// CLI types
// ---------------------------------------------------------------------------

/// ArceOS subcommands
#[derive(Subcommand)]
pub enum Command {
    /// Build ArceOS application
    Build(ArgsBuild),
    /// Build and run ArceOS application in QEMU
    Qemu(ArgsQemu),
    /// Generate a default ArceOS dynamic board config
    Defconfig(ArgsDefconfig),
    /// ArceOS board config helpers
    Config(ArgsConfig),
    /// Run ArceOS test suites
    Test(test::ArgsTest),
    /// Build and run ArceOS application with U-Boot
    Uboot(ArgsUboot),
    /// Build and run ArceOS application on a remote board
    Board(ArgsBoard),
}

#[derive(Args)]
pub struct ArgsBuild {
    #[arg(short, long)]
    pub config: Option<PathBuf>,
    #[arg(short, long)]
    pub package: Option<String>,
    #[arg(long)]
    pub arch: Option<String>,
    #[arg(short, long)]
    pub target: Option<String>,

    #[arg(long, value_name = "CPUS")]
    pub smp: Option<usize>,

    #[arg(long)]
    pub debug: bool,
}

#[derive(Args)]
pub struct ArgsQemu {
    #[command(flatten)]
    pub build: ArgsBuild,

    #[arg(long)]
    pub qemu_config: Option<PathBuf>,

    /// Override the rootfs disk image path (skips auto-download).
    #[arg(long, value_name = "IMAGE")]
    pub rootfs: Option<PathBuf>,
}

#[derive(Args)]
pub struct ArgsUboot {
    #[command(flatten)]
    pub build: ArgsBuild,

    #[arg(long)]
    pub uboot_config: Option<PathBuf>,
}

#[derive(Args)]
pub struct ArgsBoard {
    #[command(flatten)]
    pub build: ArgsBuild,

    #[arg(long = "board-config")]
    pub board_config: Option<PathBuf>,

    #[arg(short = 'b', long = "board-type")]
    pub board_type: Option<String>,

    #[arg(long)]
    pub server: Option<String>,

    #[arg(long)]
    pub port: Option<u16>,
}

#[derive(Args)]
pub struct ArgsDefconfig {
    pub board: String,
}

#[derive(Args)]
pub struct ArgsConfig {
    #[command(subcommand)]
    pub command: ConfigCommand,
}

#[derive(Subcommand)]
pub enum ConfigCommand {
    /// List available board names
    Ls,
}

// ---------------------------------------------------------------------------
// ArceOS executor
// ---------------------------------------------------------------------------

pub struct ArceOS {
    pub(super) app: AppContext,
}

impl From<&ArgsBuild> for BuildCliArgs {
    fn from(args: &ArgsBuild) -> Self {
        Self {
            config: args.config.clone(),
            package: args.package.clone(),
            arch: args.arch.clone(),
            target: args.target.clone(),
            smp: args.smp,
            debug: args.debug,
        }
    }
}

impl ArceOS {
    pub fn new() -> anyhow::Result<Self> {
        let app = AppContext::new()?;
        Ok(Self { app })
    }

    pub async fn execute(&mut self, command: Command) -> anyhow::Result<()> {
        match command {
            Command::Build(args) => self.build(args).await,
            Command::Qemu(args) => self.qemu(args).await,
            Command::Defconfig(args) => self.defconfig(args),
            Command::Config(args) => self.config(args),
            Command::Uboot(args) => self.uboot(args).await,
            Command::Board(args) => self.board(args).await,
            Command::Test(args) => self.test(args).await,
        }
    }

    async fn build(&mut self, args: ArgsBuild) -> anyhow::Result<()> {
        let request =
            self.prepare_request((&args).into(), None, None, SnapshotPersistence::Store)?;
        self.ensure_default_build_config_for_request(&request, "build")?;
        self.run_build_request(request).await
    }

    async fn qemu(&mut self, args: ArgsQemu) -> anyhow::Result<()> {
        let mut build_args = BuildCliArgs::from(&args.build);
        if build_args.package.is_none() && build_args.config.is_none() {
            // Bare `arceos qemu` is a command-level default: select the matching
            // board template, whose package and features define the runnable app.
            // Explicit package/config selectors remain fully authoritative.
            let (_, target) =
                resolve_arceos_arch_and_target(build_args.arch.clone(), build_args.target.clone())?;
            let board = board::default_qemu_board(self.app.workspace_root(), &target, None)?
                .ok_or_else(|| {
                    anyhow!(
                        "missing ArceOS QEMU default config for target `{target}` under {}",
                        board::board_dir(self.app.workspace_root())
                            .map(|path| path.display().to_string())
                            .unwrap_or_else(|_| "os/arceos/configs/board".to_string())
                    )
                })?;
            build_args.config = Some(board.path);
        }
        let request = self.prepare_request(
            build_args,
            args.qemu_config,
            None,
            SnapshotPersistence::Store,
        )?;
        self.ensure_default_build_config_for_request(&request, "qemu")?;
        if let Some(rootfs) = args.rootfs {
            rootfs::qemu_with_explicit_rootfs(self, request, rootfs).await
        } else {
            self.run_qemu_request(request).await
        }
    }

    async fn uboot(&mut self, args: ArgsUboot) -> anyhow::Result<()> {
        let request = self.prepare_request(
            (&args.build).into(),
            None,
            args.uboot_config,
            SnapshotPersistence::Store,
        )?;
        self.run_uboot_request(request).await
    }

    fn defconfig(&mut self, args: ArgsDefconfig) -> anyhow::Result<()> {
        let path = config::write_defconfig(self.app.workspace_root(), &args.board)?;
        println!("Generated {} for board {}", path.display(), args.board);
        Ok(())
    }

    fn config(&mut self, args: ArgsConfig) -> anyhow::Result<()> {
        match args.command {
            ConfigCommand::Ls => {
                for board in config::available_board_names(self.app.workspace_root())? {
                    println!("{board}");
                }
            }
        }
        Ok(())
    }

    async fn board(&mut self, args: ArgsBoard) -> anyhow::Result<()> {
        let request =
            self.prepare_request((&args.build).into(), None, None, SnapshotPersistence::Store)?;
        self.run_board_request(
            request,
            args.board_config,
            RunBoardOptions {
                board_type: args.board_type,
                server: args.server,
                port: args.port,
            },
        )
        .await
    }

    // ---- test dispatch ----

    async fn test(&mut self, args: test::ArgsTest) -> anyhow::Result<()> {
        test::test(self, args).await
    }

    // ---- internal helpers ----

    pub(super) fn prepare_request(
        &self,
        args: BuildCliArgs,
        qemu_config: Option<PathBuf>,
        uboot_config: Option<PathBuf>,
        persistence: SnapshotPersistence,
    ) -> anyhow::Result<ResolvedBuildRequest> {
        let (request, snapshot) = self.app.prepare_arceos_request(
            args,
            qemu_config,
            uboot_config,
            build::resolve_build_info_path,
        )?;
        if persistence.should_store() {
            self.app.store_arceos_snapshot(&snapshot)?;
        }
        Ok(request)
    }

    fn ensure_default_build_config_for_request(
        &self,
        request: &ResolvedBuildRequest,
        command: &str,
    ) -> anyhow::Result<()> {
        if let Some(board) = config::ensure_default_build_config_for_target(
            self.app.workspace_root(),
            &request.package,
            &request.target,
            &request.build_info_path,
        )? {
            println!(
                "generated missing ArceOS {command} build config {} from board {}",
                request.build_info_path.display(),
                board.name
            );
        }
        Ok(())
    }

    pub(super) async fn load_qemu_config(
        &mut self,
        request: &ResolvedBuildRequest,
        cargo: &Cargo,
    ) -> anyhow::Result<Option<ostool::run::qemu::QemuConfig>> {
        let qemu = match request.qemu_config.as_deref() {
            Some(path) => self
                .app
                .read_qemu_config_from_path_for_cargo(cargo, path)
                .await
                .map(Some)?,
            None => {
                let path =
                    default_qemu_config_template_path(self.app.workspace_root(), &request.arch);
                self.app
                    .read_qemu_config_from_path_for_cargo(cargo, &path)
                    .await
                    .map(Some)?
            }
        };
        Ok(qemu)
    }

    async fn load_uboot_config(
        &mut self,
        request: &ResolvedBuildRequest,
        cargo: &Cargo,
    ) -> anyhow::Result<Option<ostool::run::uboot::UbootConfig>> {
        match request.uboot_config.as_deref() {
            Some(path) => self
                .app
                .read_uboot_config_from_path_for_cargo(cargo, path)
                .await
                .map(Some),
            None => Ok(None),
        }
    }

    async fn load_board_config(
        &mut self,
        cargo: &Cargo,
        board_config_path: Option<&Path>,
    ) -> anyhow::Result<BoardRunConfig> {
        match board_config_path {
            Some(path) => {
                self.app
                    .read_board_run_config_from_path_for_cargo(cargo, path)
                    .await
            }
            None => {
                let workspace_root = self.app.workspace_root().to_path_buf();
                self.app
                    .ensure_board_run_config_in_dir_for_cargo(cargo, &workspace_root)
                    .await
            }
        }
    }

    fn validate_board_request(request: &ResolvedBuildRequest) -> anyhow::Result<()> {
        if request.build_info_path.exists() {
            board::load_build_file(&request.build_info_path)?;
        }
        Ok(())
    }

    async fn run_qemu_request(&mut self, request: ResolvedBuildRequest) -> anyhow::Result<()> {
        match build::load_arceos_build_mode(&request.build_info_path)? {
            build::ArceosBuildMode::RustStd => {
                let cargo = build::load_cargo_config(&request)?;
                self.run_qemu_request_with_cargo(request, cargo).await
            }
            build::ArceosBuildMode::AppC { app_dir, app_name } => {
                self.run_c_app_qemu_request(request, app_dir, app_name)
                    .await
            }
        }
    }

    async fn run_board_request(
        &mut self,
        request: ResolvedBuildRequest,
        board_config_path: Option<PathBuf>,
        options: RunBoardOptions,
    ) -> anyhow::Result<()> {
        self.run_board_request_with_extra_rustflags(request, board_config_path, options, &[])
            .await
    }

    pub(super) async fn run_board_request_with_extra_rustflags(
        &mut self,
        request: ResolvedBuildRequest,
        board_config_path: Option<PathBuf>,
        options: RunBoardOptions,
        extra_rustflags: &[&str],
    ) -> anyhow::Result<()> {
        Self::validate_board_request(&request)?;
        self.app.set_debug_mode(request.debug)?;
        match build::load_arceos_build_mode(&request.build_info_path)? {
            build::ArceosBuildMode::RustStd => {
                let mut cargo = build::load_cargo_config(&request)?;
                if !extra_rustflags.is_empty() {
                    crate::build::append_cargo_rustflags(&mut cargo, extra_rustflags);
                }
                let board_config = self
                    .load_board_config(&cargo, board_config_path.as_deref())
                    .await?;
                self.app
                    .board(cargo, request.build_info_path, board_config, options)
                    .await
            }
            build::ArceosBuildMode::AppC { app_dir, app_name } => {
                if !extra_rustflags.is_empty() {
                    bail!("ArceOS board extra rustflags are only supported for RustStd packages");
                }
                let cargo = build::load_c_app_cargo_config(&request)?;
                let board_config = self
                    .load_board_config(&cargo, board_config_path.as_deref())
                    .await?;
                let output = self.build_c_app_request(&request, app_dir, app_name)?;
                self.app
                    .board_prepared_elf(
                        output.elf_path,
                        cargo.to_bin,
                        request.build_info_path,
                        board_config,
                        options,
                    )
                    .await
            }
        }
    }

    async fn run_qemu_request_with_cargo(
        &mut self,
        request: ResolvedBuildRequest,
        cargo: Cargo,
    ) -> anyhow::Result<()> {
        self.app.set_debug_mode(request.debug)?;
        let mut qemu = self
            .load_qemu_config(&request, &cargo)
            .await?
            .with_context(|| {
                format!(
                    "missing ArceOS QEMU config for target `{}`; pass --qemu-config or add {}",
                    request.target,
                    default_qemu_config_template_path(self.app.workspace_root(), &request.arch)
                        .display()
                )
            })?;
        // ArceOS currently boots its default QEMU path from a fresh FAT32 image.
        // Keep this distinct from the image-managed rootfs used by StarryOS and
        // Axvisor until their runtime filesystem contracts are unified.
        crate::test::qemu::apply_smp_qemu_arg(&mut qemu, request.smp);
        rootfs::prepare_default_qemu_fat32_rootfs(self.app.workspace_root(), &qemu)?;
        let _host_http_server = start_qemu_host_http_server(&request)?;
        if load_aicp_host_probe_config(request.qemu_config.as_deref())?.is_none() {
            return self
                .app
                .qemu(cargo, request.build_info_path, Some(qemu))
                .await;
        }

        // Build before starting the probe guard. A cold target build can be
        // longer than the guest-network readiness deadline, but it is not a
        // guest reachability failure and must not consume that deadline.
        let output = self
            .app
            .build(cargo, request.build_info_path.clone())
            .await?;
        self.app
            .prepare_elf_artifact(output.elf_path().to_path_buf(), qemu.to_bin)
            .await?;
        let host_probe_guard = start_qemu_aicp_host_probe(&request, &mut qemu)?;
        let qemu_result = self.app.run_prepared_qemu(qemu, None).await;
        let probe_configured = host_probe_guard.is_some();
        let probe_result = host_probe_guard
            .as_ref()
            .and_then(HostTcpProbeGuard::take_result);
        drop(host_probe_guard);
        qemu_result?;
        match (probe_configured, probe_result) {
            (false, _) => Ok(()),
            (true, Some(result)) => result,
            (true, None) => bail!("AICP host probe produced no verdict"),
        }
    }

    async fn run_build_request(&mut self, request: ResolvedBuildRequest) -> anyhow::Result<()> {
        self.app.set_debug_mode(request.debug)?;
        match build::load_arceos_build_mode(&request.build_info_path)? {
            build::ArceosBuildMode::RustStd => {
                let cargo = build::load_cargo_config(&request)?;
                self.app
                    .build(cargo, request.build_info_path)
                    .await
                    .map(|_| ())
            }
            build::ArceosBuildMode::AppC { app_dir, app_name } => {
                let output = self.build_c_app_request(&request, app_dir, app_name)?;
                println!("Built ArceOS C app ELF: {}", output.elf_path.display());
                Ok(())
            }
        }
    }

    async fn run_uboot_request(&mut self, request: ResolvedBuildRequest) -> anyhow::Result<()> {
        self.app.set_debug_mode(request.debug)?;
        match build::load_arceos_build_mode(&request.build_info_path)? {
            build::ArceosBuildMode::RustStd => {
                let cargo = build::load_cargo_config(&request)?;
                let uboot = self.load_uboot_config(&request, &cargo).await?;
                self.app.uboot(cargo, request.build_info_path, uboot).await
            }
            build::ArceosBuildMode::AppC { app_dir, app_name } => {
                self.run_c_app_uboot_request(request, app_dir, app_name)
                    .await
            }
        }
    }

    fn build_c_app_request(
        &mut self,
        request: &ResolvedBuildRequest,
        app_dir: PathBuf,
        app_name: String,
    ) -> anyhow::Result<cbuild::ArceosCBuildOutput> {
        let workspace_root = self.app.workspace_root();
        let config = build::load_arceos_build_config(&request.build_info_path)?;
        let paths = cbuild::default_c_app_artifact_paths(workspace_root, &app_name);
        let input = cbuild::ArceosCBuildInput {
            app_dir,
            app_name,
            target_dir: paths.target_dir,
            out_dir: paths.out_dir,
            features: config.build_info.features,
        };

        cbuild::build_c_app(workspace_root, request, &input)
    }

    async fn run_c_app_qemu_request(
        &mut self,
        request: ResolvedBuildRequest,
        app_dir: PathBuf,
        app_name: String,
    ) -> anyhow::Result<()> {
        self.app.set_debug_mode(request.debug)?;
        let cargo = build::load_c_app_cargo_config(&request)?;
        let mut qemu = self
            .load_qemu_config(&request, &cargo)
            .await?
            .with_context(|| {
                format!(
                    "ArceOS C app config {} requires an explicit qemu config",
                    request.build_info_path.display()
                )
            })?;
        let output = self.build_c_app_request(&request, app_dir, app_name)?;
        // See `run_qemu_request_with_cargo`: default ArceOS QEMU keeps a FAT32 rootfs.
        crate::test::qemu::apply_smp_qemu_arg(&mut qemu, request.smp);
        rootfs::prepare_default_qemu_fat32_rootfs(self.app.workspace_root(), &qemu)?;
        self.app
            .prepare_elf_artifact(output.elf_path, qemu.to_bin)
            .await?;
        let _host_http_server = start_qemu_host_http_server(&request)?;
        self.app.run_prepared_qemu(qemu, None).await
    }

    async fn run_c_app_uboot_request(
        &mut self,
        request: ResolvedBuildRequest,
        app_dir: PathBuf,
        app_name: String,
    ) -> anyhow::Result<()> {
        self.app.set_debug_mode(request.debug)?;
        let cargo = build::load_c_app_cargo_config(&request)?;
        let uboot = self
            .load_uboot_config(&request, &cargo)
            .await?
            .with_context(|| {
                format!(
                    "ArceOS C app config {} requires an explicit uboot config",
                    request.build_info_path.display()
                )
            })?;
        let output = self.build_c_app_request(&request, app_dir, app_name)?;
        self.app.prepare_elf_artifact(output.elf_path, true).await?;
        self.app.run_prepared_uboot(uboot).await
    }
}

pub(crate) fn default_qemu_config_template_path(workspace_root: &Path, arch: &str) -> PathBuf {
    workspace_root.join(format!("os/arceos/configs/qemu/qemu-{arch}.toml"))
}

#[cfg(test)]
mod tests {
    use clap::Parser;
    use ostool::run::qemu::QemuConfig;
    use tempfile::tempdir;

    use super::*;

    #[derive(Parser)]
    struct Cli {
        #[command(subcommand)]
        command: Command,
    }

    fn parse(args: impl IntoIterator<Item = &'static str>) -> Command {
        Cli::try_parse_from(args).unwrap().command
    }

    #[test]
    fn standalone_aicp_qemu_config_enables_the_host_handshake_probe() {
        let manifest_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
        let workspace = manifest_dir
            .parent()
            .and_then(Path::parent)
            .expect("axbuild must be under the workspace scripts directory");
        let config = load_aicp_host_probe_config(Some(
            &workspace.join("apps/arceos/aicp-server/qemu-aarch64.toml"),
        ))
        .unwrap()
        .expect("standalone AICP config must enable its host probe");
        assert_eq!(config.host_port, 18_800);
        assert_eq!(config.connect_timeout_secs, 50);
    }

    #[test]
    fn aicp_host_probe_requires_a_status_reply_to_hello() {
        use std::{
            io::{Read, Write},
            net::TcpListener,
            thread,
        };

        const HEADER_LEN: usize = 32;
        const STATUS_PAYLOAD_LEN: usize = 24;
        const MAGIC: u16 = 0xA1C0;
        const VERSION: u8 = 1;
        const MSG_HELLO: u8 = 1;
        const MSG_STATUS: u8 = 3;

        let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let port = listener.local_addr().unwrap().port();
        let server = thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut request = [0_u8; HEADER_LEN];
            stream.read_exact(&mut request).unwrap();
            assert_eq!(request[3], MSG_HELLO);
            assert_eq!(u32::from_be_bytes(request[12..16].try_into().unwrap()), 1);

            let mut payload = [0_u8; STATUS_PAYLOAD_LEN];
            payload[20..24].copy_from_slice(&1_u32.to_be_bytes());
            let mut reply = [0_u8; HEADER_LEN + STATUS_PAYLOAD_LEN];
            reply[0..2].copy_from_slice(&MAGIC.to_be_bytes());
            reply[2] = VERSION;
            reply[3] = MSG_STATUS;
            reply[6..8].copy_from_slice(&(HEADER_LEN as u16).to_be_bytes());
            reply[8..12].copy_from_slice(&(STATUS_PAYLOAD_LEN as u32).to_be_bytes());
            reply[12..16].copy_from_slice(&1_u32.to_be_bytes());
            let reply_header: &[u8; HEADER_LEN] = reply[..HEADER_LEN].try_into().unwrap();
            let reply_crc = aicp_frame_crc(reply_header, &payload);
            reply[26..28].copy_from_slice(&reply_crc.to_be_bytes());
            reply[HEADER_LEN..].copy_from_slice(&payload);
            stream.write_all(&reply).unwrap();
        });

        run_aicp_hello_probe(port).unwrap();
        server.join().unwrap();
    }

    #[test]
    fn command_parses_defconfig() {
        match parse(["arceos", "defconfig", "orangepi-5-plus"]) {
            Command::Defconfig(args) => assert_eq!(args.board, "orangepi-5-plus"),
            _ => panic!("expected defconfig command"),
        }
    }

    #[test]
    fn command_parses_config_ls() {
        match parse(["arceos", "config", "ls"]) {
            Command::Config(args) => match args.command {
                ConfigCommand::Ls => {}
            },
            _ => panic!("expected config ls command"),
        }
    }

    #[test]
    fn command_parses_board() {
        match parse([
            "arceos",
            "board",
            "--config",
            "build.toml",
            "--board-config",
            "board.toml",
            "-b",
            "OrangePi-5-Plus",
            "--server",
            "10.0.0.2",
            "--port",
            "9000",
        ]) {
            Command::Board(args) => {
                assert_eq!(args.build.config, Some(PathBuf::from("build.toml")));
                assert_eq!(args.board_config, Some(PathBuf::from("board.toml")));
                assert_eq!(args.board_type.as_deref(), Some("OrangePi-5-Plus"));
                assert_eq!(args.server.as_deref(), Some("10.0.0.2"));
                assert_eq!(args.port, Some(9000));
            }
            _ => panic!("expected board command"),
        }
    }

    #[test]
    fn standard_x86_64_and_loongarch64_qemu_configs_use_uefi_boot() {
        let workspace = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");

        for (config_name, memory) in [
            ("qemu-x86_64.toml", "512M"),
            ("qemu-loongarch64.toml", "2G"),
        ] {
            let config_path = workspace.join("os/arceos/configs/qemu").join(config_name);
            let config: QemuConfig =
                toml::from_str(&std::fs::read_to_string(config_path).unwrap()).unwrap();

            assert!(config.uefi);
            assert!(config.to_bin);
            assert!(
                config.args.windows(2).any(|args| args == ["-m", memory]),
                "{config_name} must reserve {memory} for UEFI boot"
            );
        }
    }

    #[test]
    fn standard_config_templates_cover_every_supported_qemu_target() {
        let workspace = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");

        for (arch, target) in [
            ("aarch64", "aarch64-unknown-none-softfloat"),
            ("x86_64", "x86_64-unknown-none"),
            ("riscv64", "riscv64gc-unknown-none-elf"),
            ("loongarch64", "loongarch64-unknown-none-softfloat"),
        ] {
            let qemu_path = workspace.join(format!("os/arceos/configs/qemu/qemu-{arch}.toml"));
            let qemu: QemuConfig =
                toml::from_str(&std::fs::read_to_string(qemu_path).unwrap()).unwrap();
            assert!(!qemu.args.is_empty());

            let board_path = workspace.join(format!("os/arceos/configs/board/qemu-{arch}.toml"));
            let board = board::load_board_file(&board_path).unwrap();
            assert_eq!(board.package, "arceos-helloworld");
            assert_eq!(board.target, target);
            assert!(board.build_config.build_info.features.is_empty());
        }
    }

    #[test]
    fn default_qemu_config_template_uses_arceos_config_directory() {
        assert_eq!(
            default_qemu_config_template_path(Path::new("/workspace"), "aarch64"),
            PathBuf::from("/workspace/os/arceos/configs/qemu/qemu-aarch64.toml")
        );
    }

    #[test]
    fn qemu_request_starts_host_http_server_from_config() {
        let root = tempdir().unwrap();
        let qemu_config = root.path().join("qemu-x86_64.toml");
        std::fs::write(
            &qemu_config,
            r#"
args = []

[host_http_server]
port = 0
body = "fixture"
"#,
        )
        .unwrap();
        let request = ResolvedBuildRequest {
            package: "arceos-httpclient".to_string(),
            arch: "x86_64".to_string(),
            target: "x86_64-unknown-none".to_string(),
            smp: Some(1),
            debug: false,
            build_info_path: root.path().join("build.toml"),
            qemu_config: Some(qemu_config),
            uboot_config: None,
        };

        let guard = start_qemu_host_http_server(&request).unwrap();

        assert!(guard.is_some());
    }
}
