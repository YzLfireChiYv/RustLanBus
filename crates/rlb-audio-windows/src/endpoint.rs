use std::marker::PhantomData;
use std::rc::Rc;
use std::thread::{self, ThreadId};

use rlb_audio::{
    AudioError, DefaultEndpointRoles, EndpointDescriptor, EndpointDirection, EndpointFormatRange,
    EndpointState,
};
use wasapi::{Device, DeviceState};
use windows::Win32::Media::Audio::{
    DEVICE_STATE, DEVICE_STATEMASK_ALL, EDataFlow, IMMDevice, IMMDeviceEnumerator,
    MMDeviceEnumerator, eCapture, eCommunications, eConsole, eMultimedia, eRender,
};
use windows::Win32::System::Com::{
    CLSCTX_ALL, COINIT_MULTITHREADED, CoCreateInstance, CoInitializeEx, CoUninitialize,
};
use windows::core::{HSTRING, PCWSTR};

use crate::{WasapiFormat, WindowsAudioError, hresult, wasapi_error};

/// Windows default-endpoint role used for a follow-default binding.
#[derive(Clone, Copy, Debug, Default, Eq, Hash, PartialEq)]
pub enum EndpointRole {
    /// General interactive output/input used by most desktop applications.
    #[default]
    Console,
    /// Multimedia role.
    Multimedia,
    /// Communications role.
    Communications,
}

impl EndpointRole {
    /// Stable diagnostics label.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Console => "console",
            Self::Multimedia => "multimedia",
            Self::Communications => "communications",
        }
    }
}

/// User-configured endpoint binding.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum EndpointSelection {
    /// Resolve the selected Windows default role each time the supervisor polls.
    FollowDefault {
        /// Windows role to follow.
        role: EndpointRole,
    },
    /// Bind only to this opaque endpoint ID.
    Fixed {
        /// Value returned by `IMMDevice::GetId`.
        endpoint_id: String,
    },
}

impl EndpointSelection {
    /// Follows the console-role endpoint.
    #[must_use]
    pub const fn follow_default() -> Self {
        Self::FollowDefault {
            role: EndpointRole::Console,
        }
    }

    /// Pins an opaque Windows endpoint ID.
    #[must_use]
    pub fn fixed(endpoint_id: impl Into<String>) -> Self {
        Self::Fixed {
            endpoint_id: endpoint_id.into(),
        }
    }
}

/// Enumerated endpoint state and current shared-engine mix format.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct EndpointInfo {
    endpoint_id: String,
    friendly_name: String,
    direction: EndpointDirection,
    default_roles: DefaultEndpointRoles,
    state: EndpointState,
    mix_format: Option<WasapiFormat>,
}

impl EndpointInfo {
    /// Opaque stable Windows endpoint ID.
    #[must_use]
    pub fn endpoint_id(&self) -> &str {
        &self.endpoint_id
    }

    /// User-visible endpoint label.
    #[must_use]
    pub fn friendly_name(&self) -> &str {
        &self.friendly_name
    }

    /// Physical endpoint data-flow direction.
    #[must_use]
    pub const fn direction(&self) -> EndpointDirection {
        self.direction
    }

    /// Roles for which this endpoint was default at snapshot time.
    #[must_use]
    pub const fn default_roles(&self) -> DefaultEndpointRoles {
        self.default_roles
    }

    /// Current Windows endpoint state.
    #[must_use]
    pub const fn state(&self) -> EndpointState {
        self.state
    }

    /// Current shared-engine mix format, when Windows allowed it to be queried.
    #[must_use]
    pub const fn mix_format(&self) -> Option<WasapiFormat> {
        self.mix_format
    }

    /// Current mix point expressed through the platform-neutral range type.
    #[must_use]
    pub fn current_format_range(&self, engine_channels: u8) -> Option<EndpointFormatRange> {
        self.mix_format
            .map(|format| format.current_mix_range(engine_channels))
    }

    /// Builds the platform-neutral descriptor used by capture and render backends.
    pub fn to_descriptor(
        &self,
        direction: EndpointDirection,
        engine_channels: u8,
    ) -> Result<EndpointDescriptor, AudioError> {
        let range = self
            .current_format_range(engine_channels)
            .unwrap_or(EndpointFormatRange {
                minimum_sample_rate_hz: 48_000,
                maximum_sample_rate_hz: 48_000,
                minimum_channels: 1,
                maximum_channels: 2,
                supports_engine_f32: true,
            });
        EndpointDescriptor::new(
            self.endpoint_id.clone(),
            self.friendly_name.clone(),
            direction,
            self.default_roles,
            self.state,
            range,
        )
    }
}

/// Point-in-time render and capture endpoint catalog.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct EndpointCatalog {
    endpoints: Vec<EndpointInfo>,
}

impl EndpointCatalog {
    /// Returns all endpoints, including disabled, unplugged, and not-present entries.
    #[must_use]
    pub fn endpoints(&self) -> &[EndpointInfo] {
        &self.endpoints
    }

    /// Finds an endpoint by opaque stable ID.
    #[must_use]
    pub fn find(&self, endpoint_id: &str) -> Option<&EndpointInfo> {
        self.endpoints
            .iter()
            .find(|endpoint| endpoint.endpoint_id == endpoint_id)
    }

    /// Finds the active endpoint holding one default role.
    #[must_use]
    pub fn default_endpoint(
        &self,
        direction: EndpointDirection,
        role: EndpointRole,
    ) -> Option<&EndpointInfo> {
        self.endpoints.iter().find(|endpoint| {
            endpoint.direction == direction
                && endpoint.state == EndpointState::Active
                && role_is_set(endpoint.default_roles, role)
        })
    }
}

/// Supervisor action after comparing a configured binding with the active stream.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum BindingAction {
    /// The active endpoint ID is still correct.
    Unchanged,
    /// Stop the old stream and open this active endpoint on a new worker epoch/generation.
    Rebind(EndpointInfo),
    /// Keep the configured selection and publish an explicit unavailable state.
    Unavailable {
        /// Fixed ID or stable follow-default selection label.
        endpoint_id: String,
    },
}

/// A same-thread COM multi-threaded apartment guard.
///
/// The guard is deliberately `!Send`. Every COM interface created while it is
/// alive must be dropped on this thread before the guard.
pub struct ComApartment {
    owner: ThreadId,
    _not_send: PhantomData<Rc<()>>,
}

impl ComApartment {
    /// Initializes COM MTA on the calling thread.
    pub fn initialize_mta() -> Result<Self, WindowsAudioError> {
        let result = unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) };
        if result.is_err() {
            return Err(WindowsAudioError::Hresult {
                operation: "CoInitializeEx(MTA)",
                code: result.0 as u32,
            });
        }
        Ok(Self {
            owner: thread::current().id(),
            _not_send: PhantomData,
        })
    }

    pub(crate) fn ensure_owner(&self) -> Result<(), WindowsAudioError> {
        if thread::current().id() == self.owner {
            Ok(())
        } else {
            Err(WindowsAudioError::WrongThread)
        }
    }
}

impl Drop for ComApartment {
    fn drop(&mut self) {
        debug_assert_eq!(thread::current().id(), self.owner);
        unsafe { CoUninitialize() };
    }
}

/// COM-bound endpoint enumerator for the device-supervisor thread.
///
/// The raw enumerator field is dropped before the apartment guard.
pub struct EndpointEnumerator {
    enumerator: IMMDeviceEnumerator,
    apartment: ComApartment,
}

impl EndpointEnumerator {
    /// Creates a device enumerator and owns one MTA initialization count.
    pub fn new() -> Result<Self, WindowsAudioError> {
        let apartment = ComApartment::initialize_mta()?;
        let enumerator = unsafe {
            CoCreateInstance::<_, IMMDeviceEnumerator>(&MMDeviceEnumerator, None, CLSCTX_ALL)
        }
        .map_err(|error| hresult("CoCreateInstance(MMDeviceEnumerator)", error))?;
        Ok(Self {
            enumerator,
            apartment,
        })
    }

    /// Enumerates render and capture endpoints with stable IDs, labels, state,
    /// defaults, and current shared mix formats.
    pub fn enumerate(&self) -> Result<EndpointCatalog, WindowsAudioError> {
        self.apartment.ensure_owner()?;
        let defaults = self.default_ids();
        let mut endpoints = Vec::new();
        self.enumerate_direction(
            eRender,
            EndpointDirection::Render,
            &defaults,
            &mut endpoints,
        )?;
        self.enumerate_direction(
            eCapture,
            EndpointDirection::Capture,
            &defaults,
            &mut endpoints,
        )?;
        Ok(EndpointCatalog { endpoints })
    }

    fn enumerate_direction(
        &self,
        flow: EDataFlow,
        direction: EndpointDirection,
        defaults: &DefaultIds,
        output: &mut Vec<EndpointInfo>,
    ) -> Result<(), WindowsAudioError> {
        let collection = unsafe {
            self.enumerator
                .EnumAudioEndpoints(flow, DEVICE_STATE(DEVICE_STATEMASK_ALL))
        }
        .map_err(|error| hresult("IMMDeviceEnumerator::EnumAudioEndpoints", error))?;
        let count = unsafe { collection.GetCount() }
            .map_err(|error| hresult("IMMDeviceCollection::GetCount", error))?;
        output.reserve(count as usize);
        for index in 0..count {
            let raw = unsafe { collection.Item(index) }
                .map_err(|error| hresult("IMMDeviceCollection::Item", error))?;
            let device = Device::from_immdevice(raw)
                .map_err(|error| wasapi_error("Device::from_immdevice", error))?;
            let endpoint_id = device
                .get_id()
                .map_err(|error| wasapi_error("IMMDevice::GetId", error))?;
            let friendly_name = device
                .get_friendlyname()
                .or_else(|_| device.get_description())
                .unwrap_or_else(|_| endpoint_id.clone());
            let state = device
                .get_state()
                .map(map_state)
                .map_err(|error| wasapi_error("IMMDevice::GetState", error))?;
            let mix_format = device
                .get_iaudioclient()
                .and_then(|client| client.get_mixformat())
                .or_else(|_| device.get_device_format())
                .ok()
                .map(|format| WasapiFormat::from_wave_format(&format));
            output.push(EndpointInfo {
                default_roles: defaults.roles(direction, &endpoint_id),
                endpoint_id,
                friendly_name,
                direction,
                state,
                mix_format,
            });
        }
        Ok(())
    }

    fn default_ids(&self) -> DefaultIds {
        DefaultIds {
            render_console: self.default_id(eRender, eConsole),
            render_multimedia: self.default_id(eRender, eMultimedia),
            render_communications: self.default_id(eRender, eCommunications),
            capture_console: self.default_id(eCapture, eConsole),
            capture_multimedia: self.default_id(eCapture, eMultimedia),
            capture_communications: self.default_id(eCapture, eCommunications),
        }
    }

    fn default_id(
        &self,
        direction: EDataFlow,
        role: windows::Win32::Media::Audio::ERole,
    ) -> Option<String> {
        let raw = unsafe {
            self.enumerator
                .GetDefaultAudioEndpoint(direction, role)
                .ok()?
        };
        Device::from_immdevice(raw).ok()?.get_id().ok()
    }

    pub(crate) fn get_device(&self, endpoint_id: &str) -> Result<IMMDevice, WindowsAudioError> {
        self.apartment.ensure_owner()?;
        let id = HSTRING::from(endpoint_id);
        unsafe { self.enumerator.GetDevice(PCWSTR(id.as_ptr())) }.map_err(|error| {
            const ERROR_NOT_FOUND: u32 = 0x8007_0490;
            if error.code().0 as u32 == ERROR_NOT_FOUND {
                WindowsAudioError::DeviceUnavailable {
                    endpoint_id: endpoint_id.to_owned(),
                }
            } else {
                hresult("IMMDeviceEnumerator::GetDevice", error)
            }
        })
    }
}

#[derive(Default)]
struct DefaultIds {
    render_console: Option<String>,
    render_multimedia: Option<String>,
    render_communications: Option<String>,
    capture_console: Option<String>,
    capture_multimedia: Option<String>,
    capture_communications: Option<String>,
}

impl DefaultIds {
    fn roles(&self, direction: EndpointDirection, endpoint_id: &str) -> DefaultEndpointRoles {
        let ids = match direction {
            EndpointDirection::Render => (
                &self.render_console,
                &self.render_multimedia,
                &self.render_communications,
            ),
            EndpointDirection::Capture => (
                &self.capture_console,
                &self.capture_multimedia,
                &self.capture_communications,
            ),
            EndpointDirection::LoopbackCapture => return DefaultEndpointRoles::default(),
        };
        DefaultEndpointRoles {
            console: ids.0.as_deref() == Some(endpoint_id),
            multimedia: ids.1.as_deref() == Some(endpoint_id),
            communications: ids.2.as_deref() == Some(endpoint_id),
        }
    }
}

pub fn resolve_selection(
    selection: &EndpointSelection,
    direction: EndpointDirection,
    catalog: &EndpointCatalog,
) -> Result<EndpointInfo, WindowsAudioError> {
    let endpoint = match selection {
        EndpointSelection::FollowDefault { role } => catalog
            .default_endpoint(direction, *role)
            .ok_or(WindowsAudioError::NoDefaultEndpoint {
                direction: direction_label(direction),
                role: role.as_str(),
            })?,
        EndpointSelection::Fixed { endpoint_id } => {
            let Some(endpoint) = catalog.find(endpoint_id) else {
                return Err(WindowsAudioError::DeviceUnavailable {
                    endpoint_id: endpoint_id.clone(),
                });
            };
            if endpoint.direction != direction {
                return Err(WindowsAudioError::DirectionMismatch {
                    endpoint_id: endpoint_id.clone(),
                });
            }
            endpoint
        }
    };
    if endpoint.state != EndpointState::Active {
        return Err(WindowsAudioError::DeviceUnavailable {
            endpoint_id: endpoint.endpoint_id.clone(),
        });
    }
    Ok(endpoint.clone())
}

pub fn poll_binding(
    selection: &EndpointSelection,
    direction: EndpointDirection,
    active_endpoint_id: Option<&str>,
    catalog: &EndpointCatalog,
) -> BindingAction {
    match resolve_selection(selection, direction, catalog) {
        Ok(endpoint) if active_endpoint_id == Some(endpoint.endpoint_id()) => {
            BindingAction::Unchanged
        }
        Ok(endpoint) => BindingAction::Rebind(endpoint),
        Err(WindowsAudioError::DeviceUnavailable { endpoint_id }) => {
            BindingAction::Unavailable { endpoint_id }
        }
        Err(WindowsAudioError::NoDefaultEndpoint { role, .. }) => BindingAction::Unavailable {
            endpoint_id: format!("default:{}:{role}", direction_label(direction)),
        },
        Err(_) => BindingAction::Unavailable {
            endpoint_id: selection_label(selection, direction),
        },
    }
}

fn selection_label(selection: &EndpointSelection, direction: EndpointDirection) -> String {
    match selection {
        EndpointSelection::FollowDefault { role } => {
            format!("default:{}:{}", direction_label(direction), role.as_str())
        }
        EndpointSelection::Fixed { endpoint_id } => endpoint_id.clone(),
    }
}

fn role_is_set(roles: DefaultEndpointRoles, role: EndpointRole) -> bool {
    match role {
        EndpointRole::Console => roles.console,
        EndpointRole::Multimedia => roles.multimedia,
        EndpointRole::Communications => roles.communications,
    }
}

fn direction_label(direction: EndpointDirection) -> &'static str {
    match direction {
        EndpointDirection::Capture => "capture",
        EndpointDirection::Render | EndpointDirection::LoopbackCapture => "render",
    }
}

fn map_state(state: DeviceState) -> EndpointState {
    match state {
        DeviceState::Active => EndpointState::Active,
        DeviceState::Disabled => EndpointState::Disabled,
        DeviceState::NotPresent => EndpointState::NotPresent,
        DeviceState::Unplugged => EndpointState::Unplugged,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn endpoint(
        id: &str,
        direction: EndpointDirection,
        state: EndpointState,
        is_default: bool,
    ) -> EndpointInfo {
        EndpointInfo {
            endpoint_id: id.to_owned(),
            friendly_name: id.to_owned(),
            direction,
            default_roles: DefaultEndpointRoles {
                console: is_default,
                ..DefaultEndpointRoles::default()
            },
            state,
            mix_format: None,
        }
    }

    #[test]
    fn follow_default_requests_rebind_only_after_id_change() {
        let catalog = EndpointCatalog {
            endpoints: vec![
                endpoint(
                    "render-a",
                    EndpointDirection::Render,
                    EndpointState::Active,
                    false,
                ),
                endpoint(
                    "render-b",
                    EndpointDirection::Render,
                    EndpointState::Active,
                    true,
                ),
            ],
        };
        let selection = EndpointSelection::follow_default();
        assert_eq!(
            poll_binding(
                &selection,
                EndpointDirection::Render,
                Some("render-a"),
                &catalog
            ),
            BindingAction::Rebind(catalog.endpoints[1].clone())
        );
        assert_eq!(
            poll_binding(
                &selection,
                EndpointDirection::Render,
                Some("render-b"),
                &catalog
            ),
            BindingAction::Unchanged
        );
    }

    #[test]
    fn fixed_missing_or_inactive_endpoint_stays_unavailable() {
        let catalog = EndpointCatalog {
            endpoints: vec![endpoint(
                "pinned",
                EndpointDirection::Capture,
                EndpointState::Unplugged,
                false,
            )],
        };
        let selection = EndpointSelection::fixed("pinned");
        assert_eq!(
            poll_binding(
                &selection,
                EndpointDirection::Capture,
                Some("other"),
                &catalog
            ),
            BindingAction::Unavailable {
                endpoint_id: "pinned".to_owned()
            }
        );
        let absent = EndpointSelection::fixed("absent");
        assert_eq!(
            poll_binding(&absent, EndpointDirection::Capture, None, &catalog),
            BindingAction::Unavailable {
                endpoint_id: "absent".to_owned()
            }
        );
    }

    #[test]
    fn endpoint_id_enforces_data_flow() {
        let catalog = EndpointCatalog {
            endpoints: vec![endpoint(
                "render",
                EndpointDirection::Render,
                EndpointState::Active,
                false,
            )],
        };
        assert!(matches!(
            resolve_selection(
                &EndpointSelection::fixed("render"),
                EndpointDirection::Capture,
                &catalog
            ),
            Err(WindowsAudioError::DirectionMismatch { .. })
        ));
    }
}
