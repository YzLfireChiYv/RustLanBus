//! Common DNS-SD advertisement contract shared with the headless daemon.
pub use rlb_contact::advertisement::{
    AdvertisedRoom, DiscoveryConfig, DiscoveryError, InterfaceSelector,
    PROTOCOL_VERSION, SERVICE_TYPE, TXT_ADDRESS, TXT_INSTANCE, TXT_NAME, TXT_VERSION,
    display_name_ok, validate_config,
};
