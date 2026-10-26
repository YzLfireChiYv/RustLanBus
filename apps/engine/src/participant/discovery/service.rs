//! mDNS service-info construction and resolved-record adaptation.
//!
//! Address bytes come from the library's scoped addresses. The saved card
//! address is not an input.

use std::net::IpAddr;

use mdns_sd::{ResolvedService, ScopedIp, TxtProperties};

use super::contract::{
    SERVICE_TYPE, TXT_ADDRESS, TXT_INSTANCE, TXT_NAME, TXT_VERSION,
};
use super::map::{ResolvedRecord, SuppliedAddress};
pub(crate) use rlb_contact::advertisement::{
    build_service_info, build_service_info_bound, build_service_info_bound_all, to_if_kind,
};

pub(crate) fn is_our_service_type(service_type: &str) -> bool {
    service_type.eq_ignore_ascii_case(SERVICE_TYPE)
}

/// Adapt one library resolution.
///
/// Returns none while the library still has no address, so an unresolved
/// service cannot enter the candidate pool as an empty endpoint.
pub(crate) fn record_from_resolved(resolved: &ResolvedService) -> Option<ResolvedRecord> {
    if !resolved.is_valid() {
        return None;
    }
    Some(ResolvedRecord {
        fullname: resolved.fullname.clone(),
        port: resolved.port,
        addresses: resolved
            .addresses
            .iter()
            .filter_map(supplied_address)
            .collect(),
        version: text_value(&resolved.txt_properties, TXT_VERSION),
        display_name: text_value(&resolved.txt_properties, TXT_NAME),
        instance_text: text_value(&resolved.txt_properties, TXT_INSTANCE),
        bound_address: resolved
            .txt_properties
            .get_property_val_str(TXT_ADDRESS)
            .map(str::to_owned),
    })
}

pub(crate) fn supplied_address(address: &ScopedIp) -> Option<SuppliedAddress> {
    Some(match address {
        ScopedIp::V4(address) => SuppliedAddress {
            ip: IpAddr::V4(*address.addr()),
            interface_index: 0,
        },
        ScopedIp::V6(address) => SuppliedAddress {
            ip: IpAddr::V6(*address.addr()),
            interface_index: address.scope_id().index,
        },
        _ => return None,
    })
}

fn text_value(properties: &TxtProperties, key: &str) -> Option<String> {
    let value = properties.get_property_val_str(key)?;
    if value.is_empty() {
        None
    } else {
        Some(value.to_owned())
    }
}

#[cfg(test)]
mod tests {
    use std::net::{Ipv4Addr, Ipv6Addr, SocketAddr};

    use mdns_sd::ScopedIp;
    use super::super::contract::{AdvertisedRoom, PROTOCOL_VERSION};
    use rlb_model::RoomId;

    use super::super::map::{BrowseSignal, CandidatePool, normalize_contact};
    use super::*;

    fn room() -> AdvertisedRoom {
        AdvertisedRoom {
            instance_id: RoomId::from_bytes([0x0a; 16]).expect("nonzero instance marker"),
            display_name: "Studio Two".to_owned(),
            port: 47101,
        }
    }

    #[test]
    fn advertisement_keeps_the_caption_out_of_the_dns_label() {
        let room = room();
        let info = build_service_info(&room, &[]).expect("advertisement");
        let instance = room.instance_id.to_string();
        let mut keys: Vec<&str> = info
            .get_properties()
            .iter()
            .map(|property| property.key())
            .collect();
        keys.sort_unstable();
        assert_eq!(keys, vec![TXT_INSTANCE, TXT_NAME, TXT_VERSION]);
        assert_eq!(
            info.get_property_val_str(TXT_VERSION),
            Some(PROTOCOL_VERSION)
        );
        assert_eq!(
            info.get_property_val_str(TXT_NAME),
            Some(room.display_name.as_str())
        );
        assert_eq!(
            info.get_property_val_str(TXT_INSTANCE),
            Some(instance.as_str())
        );
        assert!(info.get_property_val_str("password").is_none());
        assert!(info.get_property_val_str("rules").is_none());
        assert_eq!(info.get_fullname(), format!("{instance}.{SERVICE_TYPE}"));
        assert!(!info.get_fullname().contains("Studio"));
        assert_eq!(info.get_hostname(), format!("rlb-{instance}.local."));
        assert_eq!(info.get_port(), 47101);
        assert!(info.get_addresses().is_empty());
        assert!(info.is_addr_auto());
        assert!(info.requires_probe());
        assert!(record_from_resolved(&info.as_resolved_service()).is_none());
    }

    #[test]
    fn scoped_address_adapter_uses_the_interface_index() {
        let link_local = supplied_address(&ScopedIp::from(IpAddr::V6(Ipv6Addr::new(
            0xfe80, 0, 0, 0, 0, 0, 0, 1,
        ))))
        .expect("ipv6");
        assert_eq!(link_local.interface_index, 0);
        assert!(normalize_contact(link_local.ip, link_local.interface_index, 47101).is_none());

        let global = supplied_address(&ScopedIp::from(IpAddr::V6(Ipv6Addr::new(
            0x2001, 0x0db8, 0, 0, 0, 0, 0, 1,
        ))))
        .expect("ipv6");
        assert_eq!(
            normalize_contact(global.ip, global.interface_index, 47101),
            Some(SocketAddr::from((
                Ipv6Addr::new(0x2001, 0x0db8, 0, 0, 0, 0, 0, 1),
                47101
            )))
        );

        let version_four =
            supplied_address(&ScopedIp::from(IpAddr::V4(Ipv4Addr::new(192, 0, 2, 10))))
                .expect("ipv4");
        let record = ResolvedRecord {
            fullname: format!("label.{SERVICE_TYPE}"),
            port: 47101,
            addresses: vec![version_four],
            version: Some(PROTOCOL_VERSION.to_owned()),
            display_name: Some("Studio Two".to_owned()),
            instance_text: Some(room().instance_id.to_string()),
            bound_address: None,
        };
        let found = CandidatePool::default().apply(BrowseSignal::Resolved(record));
        assert_eq!(
            found[0].candidates,
            vec!["192.0.2.10:47101".parse::<SocketAddr>().expect("socket")]
        );
    }
}
