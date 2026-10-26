# Independent OpenWrt management

`rustlanbus-openwrt-admin` is a short-lived local management command for the
RustLanBus native package. The audio/contact daemon remains a separate process.
Stopping that daemon does not remove configuration or management access.

The application owns one UCI file, `/etc/config/gw_app_rustlanbus`, its existing
procd service, and its own GL Tools page/CGI. Management uses the official GL
administrator session. There is no additional management listener, account,
gateway-controller dependency, or private application-installation platform.

The deployment adapter supplies authentication, a cross-process native lock,
request length limits and CGI timeouts. This command accepts only `status` or
`request`; the latter reads bounded JSON from standard input. Callers must hold
the same application lock when issuing local commands.

## Contract

Status reports configuration separately from the actual native service, process
and ready state. Runtime files must match the process PID, start ticks,
executable and fixed arguments. An inactive service can still be configured.
Passwords are represented only by `password_set`.

JSON requests have an `action`: `start`, `stop`, `restart`, `enable`, `disable`,
or `save`. Save also requires the last status `revision` and a `config` object:
`enabled`, `bind`, `return_bind`, `room_name`, `password_action`, `password`.
Password action explicitly keeps, replaces or clears the saved value. Unknown
fields are rejected. An empty or omitted return binding disables the optional
return host. Socket fields use an explicit IP and nonzero port.

`enabled` is the boot preference. Start/stop do not change it; enable registers
boot startup without starting the service, and disable stops and unregisters it.
Changing only the preference during save does not stop a temporary run. Saving
room/password/bind changes restarts only an already active application. Failed
changes attempt to restore the original configuration bytes and runtime intent;
external file changes are preserved and incomplete restoration is reported.

## Native lifecycle and scope

The ordinary package's init honors its boot preference during automatic starts.
The manager supplies `RUSTLANBUS_TEMPORARY_START=1` only to explicit start/restart
commands. Controlled native upgrades retain the current configuration and actual
runtime/rc intent, skip SDK postinst with `IPKG_NO_SCRIPT=1`, and restore the
recorded intent. Native prerm can still stop the application's old process.
Keep a compatible previous IPK and the configuration snapshot for recovery.

The native GL view, CGI, procd recipe and deployment records belong to the
router integration repository. They use the vendor's existing extension points
and never require reviving a retired gateway controller. UI and listening tests
are owner-operated. Audio wire and directory protocol are unchanged by this
management adapter.
