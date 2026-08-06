extern crate alloc;

use alloc::{vec, vec::Vec};
use core::time::Duration;

use uefi::{
    Error, Guid, Status,
    boot::{self, OpenProtocolAttributes, OpenProtocolParams, SearchType},
    proto::{
        ProtocolPointer,
        network::{
            http::{Http, HttpBinding, HttpHelper},
            ip4config2::Ip4Config2,
            pxe::BaseCode,
            snp::SimpleNetwork,
        },
    },
};
use uefi_raw::protocol::network::{
    dhcp4::Dhcp4Protocol,
    http::{HttpProtocol, HttpStatusCode},
    ip4_config2::Ip4Config2Protocol,
    pxe::PxeBaseCodeProtocol,
    snp::SimpleNetworkProtocol,
    tcp4::Tcp4Protocol,
};

const MAX_KERNEL_DOWNLOAD_SIZE: usize = 256 * 1024 * 1024;
const HTTP_RETRY_LIMIT: usize = 8;
const HTTP_RETRY_STALL: Duration = Duration::from_millis(250);
const KERNEL_PROGRESS_STEP_PERCENT: usize = 1;
const KERNEL_PROGRESS_BAR_WIDTH: usize = 50;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DownloadError {
    NoHttpBinding,
    HttpUnavailable,
    ConfigureFailed,
    RequestFailed,
    ResponseFailed,
    BodyTooLarge,
    UnexpectedStatus,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KernelLoadError {
    ZeroSize,
    SizeTooLarge,
    Download(DownloadError),
    SizeMismatch,
}

pub fn download_sized_body(url: &str, expected_size: u64) -> Result<Vec<u8>, KernelLoadError> {
    let expected_size = checked_kernel_size(expected_size)?;
    crate::logln!("body_download_start: size={}", expected_size);
    let mut body = vec![0; expected_size];
    let received = download_body_to_addr(url, body.as_mut_ptr(), expected_size)
        .map_err(KernelLoadError::Download)?;
    if received != expected_size {
        return Err(KernelLoadError::SizeMismatch);
    }
    Ok(body)
}

fn download_body_to_addr(
    url: &str,
    dst: *mut u8,
    expected_size: usize,
) -> Result<usize, DownloadError> {
    prepare_network();

    let mut client = HttpClient::new()?;
    let mut downloaded = 0usize;
    let mut progress = DownloadProgress::new(expected_size);
    progress.print(downloaded);

    client.request_get(url)?;
    let first = client.response_first()?;
    if first.status != HttpStatusCode::STATUS_200_OK {
        progress.finish_line();
        crate::logln!(
            "http_unexpected_status: {:?} first_body_len={}",
            first.status,
            first.body.len()
        );
        return Err(DownloadError::UnexpectedStatus);
    }
    downloaded = append_download_chunk(dst, expected_size, downloaded, &first.body)?;
    progress.maybe_print(downloaded);

    while downloaded < expected_size {
        let chunk = match retry_http(|| client.response_more_vec()) {
            Ok(chunk) => chunk,
            Err(err) => {
                progress.finish_line();
                crate::logln!(
                    "kernel_download_stopped: offset={} error={err:?}",
                    downloaded
                );
                return Err(err);
            }
        };
        if chunk.is_empty() {
            progress.finish_line();
            crate::logln!("kernel_download_stopped: offset={} zero_chunk", downloaded);
            return Err(DownloadError::ResponseFailed);
        }
        downloaded = append_download_chunk(dst, expected_size, downloaded, &chunk)?;
        progress.maybe_print(downloaded);
    }

    progress.finish_line();
    Ok(downloaded)
}

fn append_download_chunk(
    dst: *mut u8,
    expected_size: usize,
    downloaded: usize,
    chunk: &[u8],
) -> Result<usize, DownloadError> {
    let next = downloaded
        .checked_add(chunk.len())
        .ok_or(DownloadError::BodyTooLarge)?;
    if next > expected_size {
        return Err(DownloadError::BodyTooLarge);
    }
    unsafe {
        core::ptr::copy_nonoverlapping(chunk.as_ptr(), dst.add(downloaded), chunk.len());
    }
    Ok(next)
}

fn prepare_network() {
    log_network_protocol_snapshot("before_connect");
    connect_boot_controllers();
    log_network_protocol_snapshot("after_connect");

    let handles = match boot::find_handles::<Ip4Config2>() {
        Ok(handles) => handles,
        Err(err) => {
            crate::logln!("network_ifup_failed: {:?}", err.status());
            return;
        }
    };

    let mut last_error = None;
    for (index, handle) in handles.iter().copied().enumerate() {
        let mut protocol = match unsafe {
            boot::open_protocol::<Ip4Config2>(
                OpenProtocolParams {
                    handle,
                    agent: boot::image_handle(),
                    controller: None,
                },
                OpenProtocolAttributes::GetProtocol,
            )
        } {
            Ok(protocol) => protocol,
            Err(err) => {
                last_error = Some(err.status());
                continue;
            }
        };

        match protocol.ifup() {
            Ok(()) => {
                crate::logln!("network_ifup_ok: handle={}", index);
                return;
            }
            Err(err) => {
                crate::logln!(
                    "network_ifup_handle_failed: handle={} status={:?}",
                    index,
                    err.status()
                );
                last_error = Some(err.status());
            }
        }
    }

    if let Some(status) = last_error {
        crate::logln!("network_ifup_failed: {status:?}");
    } else {
        crate::logln!("network_ifup_failed: no IPv4 config handle");
    }
}

fn log_network_protocol_snapshot(phase: &str) {
    let all_handles = handle_count(SearchType::AllHandles);
    let simple_network = protocol_handle_count::<SimpleNetwork>();
    let pxe_base_code = protocol_handle_count::<BaseCode>();
    let ip4_config2 = protocol_handle_count::<Ip4Config2>();
    let http = protocol_handle_count::<Http>();
    let http_binding = protocol_handle_count::<HttpBinding>();
    crate::logln!(
        "network_protocols_{phase}: all_handles={} snp={} pxe={} ip4_config2={} http={} \
         http_binding={}",
        format_handle_count(all_handles),
        format_handle_count(simple_network),
        format_handle_count(pxe_base_code),
        format_handle_count(ip4_config2),
        format_handle_count(http),
        format_handle_count(http_binding)
    );
    log_raw_network_protocol_snapshot(phase);
}

fn protocol_handle_count<P: ProtocolPointer + ?Sized>() -> Result<usize, Error> {
    handle_count(SearchType::from_proto::<P>())
}

fn log_raw_network_protocol_snapshot(phase: &str) {
    crate::logln!(
        "network_raw_protocols_{phase}: snp={} pxe={} ip4_config2={} dhcp4={} dhcp4_binding={} \
         tcp4={} tcp4_binding={} http={} http_binding={}",
        format_handle_count(guid_handle_count(&SimpleNetworkProtocol::GUID)),
        format_handle_count(guid_handle_count(&PxeBaseCodeProtocol::GUID)),
        format_handle_count(guid_handle_count(&Ip4Config2Protocol::GUID)),
        format_handle_count(guid_handle_count(&Dhcp4Protocol::GUID)),
        format_handle_count(guid_handle_count(&Dhcp4Protocol::SERVICE_BINDING_GUID)),
        format_handle_count(guid_handle_count(&Tcp4Protocol::GUID)),
        format_handle_count(guid_handle_count(&Tcp4Protocol::SERVICE_BINDING_GUID)),
        format_handle_count(guid_handle_count(&HttpProtocol::GUID)),
        format_handle_count(guid_handle_count(&HttpProtocol::SERVICE_BINDING_GUID))
    );
}

fn guid_handle_count(guid: &Guid) -> Result<usize, Error> {
    handle_count(SearchType::ByProtocol(guid))
}

fn handle_count(search_type: SearchType<'_>) -> Result<usize, Error> {
    boot::locate_handle_buffer(search_type).map(|handles| handles.len())
}

fn format_handle_count(result: Result<usize, Error>) -> HandleCountFormat {
    HandleCountFormat(result)
}

struct HandleCountFormat(Result<usize, Error>);

impl core::fmt::Display for HandleCountFormat {
    fn fmt(&self, formatter: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match &self.0 {
            Ok(count) => write!(formatter, "{count}"),
            Err(err) => write!(formatter, "error({:?})", err.status()),
        }
    }
}

fn connect_boot_controllers() {
    let handles = match boot::locate_handle_buffer(SearchType::AllHandles) {
        Ok(handles) => handles,
        Err(err) => {
            crate::logln!("network_connect_controllers_failed: {:?}", err.status());
            return;
        }
    };

    let mut connected = 0usize;
    let mut ignored = 0usize;
    let mut last_error = None;
    for handle in handles.iter().copied() {
        match boot::connect_controller(handle, None, None, true) {
            Ok(()) => connected += 1,
            Err(err) if err.status() == Status::NOT_FOUND => ignored += 1,
            Err(err) => last_error = Some(err.status()),
        }
    }

    if connected > 0 || last_error.is_some() {
        crate::logln!(
            "network_connect_controllers: connected={} ignored={} last_error={:?}",
            connected,
            ignored,
            last_error
        );
    }
}

struct DownloadProgress {
    expected_size: usize,
    next_percent: usize,
}

impl DownloadProgress {
    fn new(expected_size: usize) -> Self {
        Self {
            expected_size,
            next_percent: KERNEL_PROGRESS_STEP_PERCENT,
        }
    }

    fn maybe_print(&mut self, downloaded: usize) {
        let percent = download_percent(downloaded, self.expected_size);
        if percent >= self.next_percent || downloaded == self.expected_size {
            self.print(downloaded);
            while self.next_percent <= percent {
                self.next_percent += KERNEL_PROGRESS_STEP_PERCENT;
            }
        }
    }

    fn print(&self, downloaded: usize) {
        let percent = download_percent(downloaded, self.expected_size);
        let filled = percent.saturating_mul(KERNEL_PROGRESS_BAR_WIDTH) / 100;
        crate::log!("\rdownload: [");
        for index in 0..KERNEL_PROGRESS_BAR_WIDTH {
            crate::log!("{}", if index < filled { "#" } else { "-" });
        }
        crate::log!("] {:>3}% ", percent);
        print_human_size(downloaded);
        crate::log!("/");
        print_human_size(self.expected_size);
        crate::log!("    ");
    }

    fn finish_line(&self) {
        crate::logln!("");
    }
}

fn download_percent(downloaded: usize, expected_size: usize) -> usize {
    downloaded
        .saturating_mul(100)
        .checked_div(expected_size)
        .unwrap_or(0)
}

fn print_human_size(bytes: usize) {
    const KIB: usize = 1024;
    const MIB: usize = 1024 * 1024;

    if bytes >= MIB {
        print_fixed_2(bytes, MIB);
        crate::log!(" MiB");
    } else if bytes >= KIB {
        print_fixed_2(bytes, KIB);
        crate::log!(" KiB");
    } else {
        crate::log!("{} B", bytes);
    }
}

fn print_fixed_2(value: usize, unit: usize) {
    let whole = value / unit;
    let hundredths = value % unit * 100 / unit;
    crate::log!("{}.", whole);
    if hundredths < 10 {
        crate::log!("0");
    }
    crate::log!("{}", hundredths);
}

fn retry_http<T>(mut op: impl FnMut() -> Result<T, DownloadError>) -> Result<T, DownloadError> {
    let mut last_error = None;
    for attempt in 1..=HTTP_RETRY_LIMIT {
        match op() {
            Ok(value) => return Ok(value),
            Err(err) => {
                last_error = Some(err);
                if attempt < HTTP_RETRY_LIMIT {
                    boot::stall(HTTP_RETRY_STALL);
                }
            }
        }
    }
    Err(last_error.expect("retry loop always runs at least once"))
}

struct HttpClient {
    helper: HttpHelper,
}

impl HttpClient {
    fn new() -> Result<Self, DownloadError> {
        let handles = match boot::find_handles::<HttpBinding>() {
            Ok(handles) => handles,
            Err(err) => {
                crate::logln!("http_binding_find_failed: {:?}", err.status());
                return Err(DownloadError::HttpUnavailable);
            }
        };
        crate::logln!("http_binding_handles: count={}", handles.len());

        let mut last_error = None;
        for (index, nic_handle) in handles.iter().copied().enumerate() {
            crate::logln!("http_helper_open: handle={}", index);
            let mut helper = match HttpHelper::new(nic_handle) {
                Ok(helper) => helper,
                Err(err) => {
                    crate::logln!(
                        "http_helper_open_failed: handle={} status={:?}",
                        index,
                        err.status()
                    );
                    last_error = Some(DownloadError::HttpUnavailable);
                    continue;
                }
            };

            match helper.configure() {
                Ok(()) => {
                    crate::logln!("http_helper_configured: handle={}", index);
                    return Ok(Self { helper });
                }
                Err(err) => {
                    crate::logln!(
                        "http_helper_configure_failed: handle={} status={:?}",
                        index,
                        err.status()
                    );
                    last_error = Some(DownloadError::ConfigureFailed);
                }
            }
        }

        Err(last_error.unwrap_or(DownloadError::NoHttpBinding))
    }

    fn request_get(&mut self, url: &str) -> Result<(), DownloadError> {
        self.helper
            .request_get(url)
            .map_err(|_| DownloadError::RequestFailed)
    }

    fn response_first(
        &mut self,
    ) -> Result<uefi::proto::network::http::HttpHelperResponse, DownloadError> {
        self.helper
            .response_first(true)
            .map_err(|_| DownloadError::ResponseFailed)
    }

    fn response_more_vec(&mut self) -> Result<Vec<u8>, DownloadError> {
        let mut body = Vec::new();
        self.helper
            .response_more(&mut body)
            .map_err(|_| DownloadError::ResponseFailed)?;
        Ok(body)
    }
}

fn checked_kernel_size(expected_size: u64) -> Result<usize, KernelLoadError> {
    if expected_size == 0 {
        return Err(KernelLoadError::ZeroSize);
    }
    if expected_size > MAX_KERNEL_DOWNLOAD_SIZE as u64 {
        return Err(KernelLoadError::SizeTooLarge);
    }
    Ok(expected_size as usize)
}
