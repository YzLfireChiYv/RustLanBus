use std::io;

use socket2::Socket;

#[cfg(windows)]
mod imp {
    use std::io;
    use std::mem;
    use std::os::windows::io::AsRawSocket;

    use socket2::Socket;

    const SOCKET_ERROR: i32 = -1;
    const IPPROTO_IP: i32 = 0;
    const SOL_SOCKET: i32 = 0xffff;
    const IP_DONTFRAGMENT: i32 = 14;
    const SO_EXCLUSIVEADDRUSE: i32 = -5;
    const WSAEMSGSIZE: i32 = 10_040;

    #[link(name = "ws2_32")]
    unsafe extern "system" {
        #[link_name = "setsockopt"]
        fn win_setsockopt(
            socket: usize,
            level: i32,
            option_name: i32,
            option_value: *const u8,
            option_length: i32,
        ) -> i32;

        #[link_name = "getsockopt"]
        fn win_getsockopt(
            socket: usize,
            level: i32,
            option_name: i32,
            option_value: *mut u8,
            option_length: *mut i32,
        ) -> i32;
    }

    pub(crate) fn set_dont_fragment(socket: &Socket) -> io::Result<()> {
        set_u32_option(socket, IPPROTO_IP, IP_DONTFRAGMENT, 1)
    }

    pub(crate) fn dont_fragment(socket: &Socket) -> io::Result<bool> {
        get_u32_option(socket, IPPROTO_IP, IP_DONTFRAGMENT).map(|value| value != 0)
    }

    pub(crate) fn set_exclusive_address_use(socket: &Socket) -> io::Result<()> {
        set_u32_option(socket, SOL_SOCKET, SO_EXCLUSIVEADDRUSE, 1)
    }

    pub(crate) fn exclusive_address_use(socket: &Socket) -> io::Result<Option<bool>> {
        get_u32_option(socket, SOL_SOCKET, SO_EXCLUSIVEADDRUSE).map(|value| Some(value != 0))
    }

    pub(crate) fn is_message_too_large(error: &io::Error) -> bool {
        error.raw_os_error() == Some(WSAEMSGSIZE)
    }

    fn set_u32_option(socket: &Socket, level: i32, name: i32, value: u32) -> io::Result<()> {
        // SAFETY: the socket handle is live for the call, and `value` points to
        // a correctly sized DWORD accepted by these Winsock options.
        let result = unsafe {
            win_setsockopt(
                socket.as_raw_socket() as usize,
                level,
                name,
                (&raw const value).cast(),
                mem::size_of_val(&value) as i32,
            )
        };
        if result == SOCKET_ERROR {
            Err(io::Error::last_os_error())
        } else {
            Ok(())
        }
    }

    fn get_u32_option(socket: &Socket, level: i32, name: i32) -> io::Result<u32> {
        let mut value = 0_u32;
        let mut length = mem::size_of_val(&value) as i32;
        // SAFETY: the socket handle is live for the call. `value` and `length`
        // provide writable storage of the size declared to Winsock.
        let result = unsafe {
            win_getsockopt(
                socket.as_raw_socket() as usize,
                level,
                name,
                (&raw mut value).cast(),
                &raw mut length,
            )
        };
        if result == SOCKET_ERROR {
            Err(io::Error::last_os_error())
        } else {
            Ok(value)
        }
    }
}

#[cfg(any(target_os = "linux", target_os = "android"))]
mod imp {
    use std::ffi::{c_int, c_void};
    use std::io;
    use std::mem;
    use std::os::fd::AsRawFd;

    use socket2::Socket;

    type SockLen = u32;

    const IPPROTO_IP: c_int = 0;
    const IP_MTU_DISCOVER: c_int = 10;
    const IP_PMTUDISC_DO: c_int = 2;
    const EMSGSIZE: i32 = 90;

    unsafe extern "C" {
        #[link_name = "setsockopt"]
        fn c_setsockopt(
            socket: c_int,
            level: c_int,
            option_name: c_int,
            option_value: *const c_void,
            option_length: SockLen,
        ) -> c_int;

        #[link_name = "getsockopt"]
        fn c_getsockopt(
            socket: c_int,
            level: c_int,
            option_name: c_int,
            option_value: *mut c_void,
            option_length: *mut SockLen,
        ) -> c_int;
    }

    pub(crate) fn set_dont_fragment(socket: &Socket) -> io::Result<()> {
        set_int_option(socket, IPPROTO_IP, IP_MTU_DISCOVER, IP_PMTUDISC_DO)
    }

    pub(crate) fn dont_fragment(socket: &Socket) -> io::Result<bool> {
        get_int_option(socket, IPPROTO_IP, IP_MTU_DISCOVER).map(|value| value == IP_PMTUDISC_DO)
    }

    pub(crate) fn set_exclusive_address_use(_socket: &Socket) -> io::Result<()> {
        Ok(())
    }

    pub(crate) fn exclusive_address_use(_socket: &Socket) -> io::Result<Option<bool>> {
        Ok(None)
    }

    pub(crate) fn is_message_too_large(error: &io::Error) -> bool {
        error.raw_os_error() == Some(EMSGSIZE)
    }

    fn set_int_option(socket: &Socket, level: c_int, name: c_int, value: c_int) -> io::Result<()> {
        // SAFETY: the file descriptor is live for the call, and `value` points
        // to an integer accepted by `IP_MTU_DISCOVER`.
        let result = unsafe {
            c_setsockopt(
                socket.as_raw_fd(),
                level,
                name,
                (&raw const value).cast(),
                mem::size_of_val(&value) as SockLen,
            )
        };
        if result == -1 {
            Err(io::Error::last_os_error())
        } else {
            Ok(())
        }
    }

    fn get_int_option(socket: &Socket, level: c_int, name: c_int) -> io::Result<c_int> {
        let mut value = 0;
        let mut length = mem::size_of_val(&value) as SockLen;
        // SAFETY: the file descriptor is live for the call. `value` and
        // `length` provide writable storage of the size declared to libc.
        let result = unsafe {
            c_getsockopt(
                socket.as_raw_fd(),
                level,
                name,
                (&raw mut value).cast(),
                &raw mut length,
            )
        };
        if result == -1 {
            Err(io::Error::last_os_error())
        } else {
            Ok(value)
        }
    }
}

#[cfg(not(any(windows, target_os = "linux", target_os = "android")))]
mod imp {
    use std::io;

    use socket2::Socket;

    pub(crate) fn set_dont_fragment(_socket: &Socket) -> io::Result<()> {
        Err(io::ErrorKind::Unsupported.into())
    }

    pub(crate) fn dont_fragment(_socket: &Socket) -> io::Result<bool> {
        Err(io::ErrorKind::Unsupported.into())
    }

    pub(crate) fn set_exclusive_address_use(_socket: &Socket) -> io::Result<()> {
        Ok(())
    }

    pub(crate) fn exclusive_address_use(_socket: &Socket) -> io::Result<Option<bool>> {
        Ok(None)
    }

    pub(crate) fn is_message_too_large(_error: &io::Error) -> bool {
        false
    }
}

pub(crate) use imp::{
    dont_fragment, exclusive_address_use, is_message_too_large, set_dont_fragment,
    set_exclusive_address_use,
};

pub(crate) fn set_ds_field(socket: &Socket, ds_field: u8) -> io::Result<()> {
    #[cfg(any(windows, target_os = "linux", target_os = "android"))]
    {
        socket.set_tos_v4(u32::from(ds_field))
    }

    #[cfg(not(any(windows, target_os = "linux", target_os = "android")))]
    {
        let _ = (socket, ds_field);
        Err(io::ErrorKind::Unsupported.into())
    }
}

pub(crate) fn ds_field(socket: &Socket) -> io::Result<u32> {
    #[cfg(any(windows, target_os = "linux", target_os = "android"))]
    {
        socket.tos_v4()
    }

    #[cfg(not(any(windows, target_os = "linux", target_os = "android")))]
    {
        let _ = socket;
        Err(io::ErrorKind::Unsupported.into())
    }
}
