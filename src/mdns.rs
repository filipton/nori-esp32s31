//! The speaker announced as `_nori-speaker._tcp`, its id and name in the TXT, for a cast plugin's
//! discovery to find.

use alloc::format;
use core::net::{Ipv4Addr, Ipv6Addr};

use edge_mdns::buf::VecBufAccess;
use edge_mdns::domain::base::Ttl;
use edge_mdns::host::{Host, Service, ServiceAnswers};
use edge_mdns::io::{self, DEFAULT_SOCKET};
use edge_mdns::HostAnswersMdnsHandler;
use edge_nal::UdpSplit;
use edge_nal_embassy::{Udp, UdpBuffers};
use embassy_net::Stack;
use embassy_sync::blocking_mutex::raw::NoopRawMutex;
use embassy_sync::signal::Signal;


/// The service a cast plugin browses for.
const SERVICE: &str = "_nori-speaker";

#[embassy_executor::task]
pub async fn announce(stack: Stack<'static>, id: alloc::string::String, name: &'static str) {
    let Some(config) = stack.config_v4() else { return };
    let buffers = UdpBuffers::<1, 1500, 1500, 2>::new();
    let udp = Udp::new(stack, &buffers);
    let (recv_buf, send_buf) = (VecBufAccess::<NoopRawMutex, 1500>::new(), VecBufAccess::<NoopRawMutex, 1500>::new());
    let Ok(mut socket) = io::bind(&udp, DEFAULT_SOCKET, Some(Ipv4Addr::UNSPECIFIED), None).await else {
        log::error!("mdns: no socket");
        return;
    };
    let (recv, send) = socket.split();

    let hostname = format!("nori-{id}");
    let host = Host { hostname: &hostname, ipv4: config.address.address(), ipv6: Ipv6Addr::UNSPECIFIED, ttl: Ttl::from_secs(120) };
    let txt = [("id", id.as_str()), ("name", name), ("kind", "speaker")];
    let service = Service {
        name: &hostname,
        priority: 0,
        weight: 0,
        service: SERVICE,
        protocol: "_tcp",
        port: crate::receive::PORT,
        service_subtypes: &[],
        txt_kvs: &txt,
    };
    let signal = Signal::<NoopRawMutex, ()>::new();
    let mdns = io::Mdns::new(Some(Ipv4Addr::UNSPECIFIED), None, recv, send, &recv_buf, &send_buf, Random, &signal);
    log::info!("announcing {hostname} as {SERVICE}._tcp");
    if let Err(e) = mdns.run(HostAnswersMdnsHandler::new(ServiceAnswers::new(&host, &service))).await {
        log::error!("mdns stopped: {e:?}");
    }
}

/// The chip's random number generator, for the responder's delays and ids.
struct Random;

impl rand_core::TryRng for Random {
    type Error = core::convert::Infallible;

    fn try_next_u32(&mut self) -> Result<u32, Self::Error> {
        Ok(esp_hal::rng::Rng::new().random())
    }

    fn try_next_u64(&mut self) -> Result<u64, Self::Error> {
        let rng = esp_hal::rng::Rng::new();
        Ok(((rng.random() as u64) << 32) | rng.random() as u64)
    }

    fn try_fill_bytes(&mut self, dst: &mut [u8]) -> Result<(), Self::Error> {
        esp_hal::rng::Rng::new().read(dst);
        Ok(())
    }
}
