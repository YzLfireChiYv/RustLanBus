//! Explicit application-scoped LAN exceptions. Inspection never creates rules.

use serde::Serialize;

#[derive(Clone, Debug, Default, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct LanFirewallState {
    /// Only this executable's two owned rules are inspected. This does not
    /// establish connectivity or override other Windows/network policies.
    pub registered: Option<bool>,
    pub program: Option<String>,
    pub last_error: Option<String>,
}

#[derive(Clone, Copy)]
enum Change {
    Inspect,
    Allow,
    Remove,
}

pub fn inspect() -> LanFirewallState {
    operate(Change::Inspect)
}
pub(crate) fn allow() -> LanFirewallState {
    operate(Change::Allow)
}
pub(crate) fn remove() -> LanFirewallState {
    operate(Change::Remove)
}

fn operate(change: Change) -> LanFirewallState {
    let mut state = LanFirewallState::default();
    #[cfg(windows)]
    if let Err(error) = native::operate(change, &mut state) {
        state.last_error = Some(error);
    }
    #[cfg(not(windows))]
    {
        let _ = change;
        state.last_error = Some("LAN firewall registration currently targets Windows.".into());
    }
    state
}

#[cfg(windows)]
mod native {
    use super::*;
    use windows::Win32::NetworkManagement::WindowsFirewall::{
        INetFwPolicy2, INetFwRule, INetFwRules, NET_FW_ACTION_ALLOW, NET_FW_IP_PROTOCOL_TCP,
        NET_FW_IP_PROTOCOL_UDP, NET_FW_PROFILE2_ALL, NET_FW_RULE_DIR_IN, NetFwPolicy2, NetFwRule,
    };
    use windows::Win32::System::Com::{
        CLSCTX_INPROC_SERVER, COINIT_MULTITHREADED, CoCreateInstance, CoInitializeEx,
        CoUninitialize,
    };
    use windows::core::BSTR;

    const GROUP: &str = "RustLanBus LocalSubnet v1";

    struct Apartment;
    impl Drop for Apartment {
        fn drop(&mut self) {
            unsafe { CoUninitialize() };
        }
    }

    pub(super) fn operate(change: Change, state: &mut LanFirewallState) -> Result<(), String> {
        let executable = std::env::current_exe()
            .map_err(|_| "The running executable path could not be resolved.".to_owned())?;
        if !executable.is_absolute() {
            return Err("The running executable path is not absolute.".into());
        }
        let program = executable
            .to_str()
            .ok_or("The running executable path cannot be represented as Unicode.")?;
        state.program = Some(program.to_owned());
        let hash = program
            .to_lowercase()
            .encode_utf16()
            .fold(0xcbf29ce484222325_u64, |hash, unit| {
                (hash ^ u64::from(unit)).wrapping_mul(0x100000001b3)
            });
        unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) }
            .ok()
            .map_err(native_error)?;
        let _apartment = Apartment;
        let policy: INetFwPolicy2 =
            unsafe { CoCreateInstance(&NetFwPolicy2, None, CLSCTX_INPROC_SERVER) }
                .map_err(native_error)?;
        let rules = unsafe { policy.Rules() }.map_err(native_error)?;
        for (label, protocol) in [
            ("TCP", NET_FW_IP_PROTOCOL_TCP.0),
            ("UDP", NET_FW_IP_PROTOCOL_UDP.0),
        ] {
            let name = BSTR::from(format!("RustLanBus-LAN-v1-{hash:016x}-{label}"));
            let existing = own_rule(&rules, &name)?;
            match change {
                Change::Inspect => {}
                Change::Allow => {
                    // Build the full narrow scope before any rule is enabled
                    // or added. Replacement repairs only this owned name.
                    let rule = make_rule(&name, program, protocol)?;
                    if existing.is_some() {
                        unsafe { rules.Remove(&name) }.map_err(native_error)?;
                    }
                    unsafe { rules.Add(&rule) }.map_err(native_error)?;
                }
                Change::Remove => {
                    if existing.is_some() {
                        unsafe { rules.Remove(&name) }.map_err(native_error)?;
                    }
                }
            }
        }
        let mut complete = true;
        for (label, protocol) in [
            ("TCP", NET_FW_IP_PROTOCOL_TCP.0),
            ("UDP", NET_FW_IP_PROTOCOL_UDP.0),
        ] {
            let name = BSTR::from(format!("RustLanBus-LAN-v1-{hash:016x}-{label}"));
            complete &= match own_rule(&rules, &name)? {
                Some(rule) => matches_scope(&rule, program, protocol)?,
                None => false,
            };
        }
        state.registered = Some(complete);
        if matches!(change, Change::Allow) && !complete {
            return Err("The application LAN rules could not be confirmed. Refresh their status or allow again.".into());
        }
        Ok(())
    }

    fn own_rule(rules: &INetFwRules, name: &BSTR) -> Result<Option<INetFwRule>, String> {
        let rule = match unsafe { rules.Item(name) } {
            Ok(rule) => rule,
            Err(error) if error.code().0 as u32 == 0x80070002 => return Ok(None),
            Err(error) => return Err(native_error(error)),
        };
        if unsafe { rule.Grouping() }
            .map_err(native_error)?
            .to_string()
            != GROUP
        {
            return Err(
                "A reserved application rule name has a different owner; no such rule was changed."
                    .into(),
            );
        }
        Ok(Some(rule))
    }

    fn make_rule(name: &BSTR, program: &str, protocol: i32) -> Result<INetFwRule, String> {
        let rule: INetFwRule = unsafe { CoCreateInstance(&NetFwRule, None, CLSCTX_INPROC_SERVER) }
            .map_err(native_error)?;
        (|| -> windows::core::Result<()> {
            unsafe {
                rule.SetName(name)?;
                rule.SetDescription(&BSTR::from(
                    "RustLanBus inbound audio and room contact from the local subnet",
                ))?;
                rule.SetGrouping(&BSTR::from(GROUP))?;
                rule.SetApplicationName(&BSTR::from(program))?;
                rule.SetProtocol(protocol)?;
                rule.SetLocalPorts(&BSTR::from("*"))?;
                rule.SetRemotePorts(&BSTR::from("*"))?;
                rule.SetLocalAddresses(&BSTR::from("*"))?;
                rule.SetRemoteAddresses(&BSTR::from("LocalSubnet"))?;
                rule.SetInterfaceTypes(&BSTR::from("All"))?;
                rule.SetDirection(NET_FW_RULE_DIR_IN)?;
                rule.SetAction(NET_FW_ACTION_ALLOW)?;
                rule.SetProfiles(NET_FW_PROFILE2_ALL.0)?;
                rule.SetEdgeTraversal(false.into())?;
                rule.SetEnabled(true.into())?;
            }
            Ok(())
        })()
        .map_err(native_error)?;
        Ok(rule)
    }

    fn matches_scope(rule: &INetFwRule, program: &str, protocol: i32) -> Result<bool, String> {
        (|| -> windows::core::Result<bool> {
            Ok(unsafe {
                rule.ApplicationName()?
                    .to_string()
                    .eq_ignore_ascii_case(program)
                    && rule.Protocol()? == protocol
                    && rule
                        .RemoteAddresses()?
                        .to_string()
                        .eq_ignore_ascii_case("LocalSubnet")
                    && rule.Direction()? == NET_FW_RULE_DIR_IN
                    && rule.Action()? == NET_FW_ACTION_ALLOW
                    && rule.Enabled()?.as_bool()
                    && !rule.EdgeTraversal()?.as_bool()
                    && rule.Profiles()? == NET_FW_PROFILE2_ALL.0
                    && rule.LocalAddresses()?.to_string() == "*"
                    && rule.LocalPorts()?.to_string() == "*"
                    && rule.RemotePorts()?.to_string() == "*"
                    && rule.ServiceName()?.is_empty()
                    && rule
                        .InterfaceTypes()?
                        .to_string()
                        .eq_ignore_ascii_case("All")
            })
        })()
        .map_err(native_error)
    }

    fn native_error(error: windows::core::Error) -> String {
        format!(
            "Windows LAN firewall registration failed (HRESULT 0x{:08x}).",
            error.code().0 as u32
        )
    }
}
