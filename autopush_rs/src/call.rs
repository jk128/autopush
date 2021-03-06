//! Implementation of calling methods/objects in python
//!
//! The main `Server` has a channel that goes back to the main python thread,
//! and that's used to send instances of `PythonCall` from the Rust thread to
//! the Python thread. Typically you won't work with `PythonCall` directly
//! though but rather the various methods on the `Server` struct, documented
//! below. Each method will return a `MyFuture` of the result, representing the
//! decoded value from Python.
//!
//! Implementation-wise what's happening here is that each function call into
//! Python creates a `futures::sync::oneshot`. The `Sender` half of this oneshot
//! is sent to Python while the `Receiver` half stays in Rust. Arguments sent to
//! Python are serialized as JSON and arguments are received from Python as JSON
//! as well, meaning that they're deserialized in Rust from JSON as well.

use std::cell::RefCell;
use std::ffi::CStr;

use futures::Future;
use futures::sync::oneshot;
use libc::c_char;
use serde::de;
use serde::ser;
use serde_json;
use uuid::Uuid;

use errors::*;
use rt::{self, UnwindGuard, AutopushError};
use protocol;
use server::Server;

#[repr(C)]
pub struct AutopushPythonCall {
    inner: UnwindGuard<Inner>,
}

struct Inner {
    input: String,
    done: RefCell<Option<Box<FnBox>>>,
}

pub struct PythonCall {
    input: String,
    output: Box<FnBox>,
}

#[no_mangle]
pub extern "C" fn autopush_python_call_input_ptr(
    call: *mut AutopushPythonCall,
    err: &mut AutopushError,
) -> *const u8 {
    unsafe { (*call).inner.catch(err, |call| call.input.as_ptr()) }
}

#[no_mangle]
pub extern "C" fn autopush_python_call_input_len(
    call: *mut AutopushPythonCall,
    err: &mut AutopushError,
) -> usize {
    unsafe { (*call).inner.catch(err, |call| call.input.len()) }
}

#[no_mangle]
pub extern "C" fn autopush_python_call_complete(
    call: *mut AutopushPythonCall,
    input: *const c_char,
    err: &mut AutopushError,
) -> i32 {
    unsafe {
        (*call).inner.catch(err, |call| {
            let input = CStr::from_ptr(input).to_str().unwrap();
            call.done.borrow_mut().take().unwrap().call(input);
        })
    }
}

#[no_mangle]
pub extern "C" fn autopush_python_call_free(call: *mut AutopushPythonCall) {
    rt::abort_on_panic(|| unsafe {
        Box::from_raw(call);
    })
}

impl AutopushPythonCall {
    pub fn new(call: PythonCall) -> AutopushPythonCall {
        AutopushPythonCall {
            inner: UnwindGuard::new(Inner {
                input: call.input,
                done: RefCell::new(Some(call.output)),
            }),
        }
    }

    fn _new<F>(input: String, f: F) -> AutopushPythonCall
    where
        F: FnOnce(&str) + Send + 'static,
    {
        AutopushPythonCall {
            inner: UnwindGuard::new(Inner {
                input: input,
                done: RefCell::new(Some(Box::new(f))),
            }),
        }
    }
}

trait FnBox: Send {
    fn call(self: Box<Self>, input: &str);
}

impl<F: FnOnce(&str) + Send> FnBox for F {
    fn call(self: Box<Self>, input: &str) {
        (*self)(input)
    }
}


#[derive(Serialize)]
#[serde(tag = "command", rename_all = "snake_case")]
enum Call {
    Hello {
        connected_at: i64,
        uaid: Option<String>,
    },

    Register {
        uaid: String,
        channel_id: String,
        message_month: String,
        key: Option<String>,
    },

    Unregister {
        uaid: String,
        channel_id: String,
        message_month: String,
        code: i32,
    },

    CheckStorage {
        uaid: String,
        message_month: String,
        include_topic: bool,
        timestamp: Option<i64>,
    },

    DeleteMessage {
        message: protocol::Notification,
        message_month: String,
    },

    IncStoragePosition {
        uaid: String,
        message_month: String,
        timestamp: i64,
    },

    DropUser { uaid: String },

    MigrateUser { uaid: String, message_month: String },

    StoreMessages {
        message_month: String,
        messages: Vec<protocol::Notification>,
    },

}

#[derive(Deserialize)]
struct PythonError {
    pub error: bool,
    pub error_msg: String,
}

#[derive(Deserialize)]
pub struct HelloResponse {
    pub uaid: Option<Uuid>,
    pub message_month: String,
    pub check_storage: bool,
    pub reset_uaid: bool,
    pub rotate_message_table: bool,
    pub connected_at: u64,
}

#[derive(Deserialize)]
#[serde(untagged)]
pub enum RegisterResponse {
    Success { endpoint: String },

    Error {
        error_msg: String,
        error: bool,
        status: u32,
    },
}

#[derive(Deserialize)]
#[serde(untagged)]
pub enum UnRegisterResponse {
    Success { success: bool },

    Error {
        error_msg: String,
        error: bool,
        status: u32,
    },
}

#[derive(Deserialize)]
pub struct CheckStorageResponse {
    pub include_topic: bool,
    pub messages: Vec<protocol::Notification>,
    pub timestamp: Option<i64>,
}

#[derive(Deserialize)]
pub struct DeleteMessageResponse {
    pub success: bool,
}

#[derive(Deserialize)]
pub struct IncStorageResponse {
    pub success: bool,
}

#[derive(Deserialize)]
pub struct DropUserResponse {
    pub success: bool,
}

#[derive(Deserialize)]
pub struct MigrateUserResponse {
    pub message_month: String,
}

#[derive(Deserialize)]
pub struct StoreMessagesResponse {
    pub success: bool,
}


impl Server {
    pub fn hello(&self, connected_at: &u64, uaid: Option<&Uuid>) -> MyFuture<HelloResponse> {
        let ms = *connected_at as i64;
        let (call, fut) = PythonCall::new(&Call::Hello {
            connected_at: ms,
            uaid: if let Some(uuid) = uaid {
                Some(uuid.simple().to_string())
            } else {
                None
            },
        });
        self.send_to_python(call);
        return fut;
    }

    pub fn register(
        &self,
        uaid: String,
        message_month: String,
        channel_id: String,
        key: Option<String>,
    ) -> MyFuture<RegisterResponse> {
        let (call, fut) = PythonCall::new(&Call::Register {
            uaid: uaid,
            message_month: message_month,
            channel_id: channel_id,
            key: key,
        });
        self.send_to_python(call);
        return fut;
    }

    pub fn unregister(
        &self,
        uaid: String,
        message_month: String,
        channel_id: String,
        code: i32,
    ) -> MyFuture<UnRegisterResponse> {
        let (call, fut) = PythonCall::new(&Call::Unregister {
            uaid: uaid,
            message_month: message_month,
            channel_id: channel_id,
            code: code,
        });
        self.send_to_python(call);
        return fut;
    }

    pub fn check_storage(
        &self,
        uaid: String,
        message_month: String,
        include_topic: bool,
        timestamp: Option<i64>,
    ) -> MyFuture<CheckStorageResponse> {
        let (call, fut) = PythonCall::new(&Call::CheckStorage {
            uaid: uaid,
            message_month: message_month,
            include_topic: include_topic,
            timestamp: timestamp,
        });
        self.send_to_python(call);
        return fut;
    }

    pub fn increment_storage(
        &self,
        uaid: String,
        message_month: String,
        timestamp: i64,
    ) -> MyFuture<IncStorageResponse> {
        let (call, fut) = PythonCall::new(&Call::IncStoragePosition {
            uaid: uaid,
            message_month: message_month,
            timestamp: timestamp,
        });
        self.send_to_python(call);
        return fut;
    }

    pub fn delete_message(
        &self,
        message_month: String,
        notif: protocol::Notification,
    ) -> MyFuture<DeleteMessageResponse> {
        let (call, fut) = PythonCall::new(&Call::DeleteMessage {
            message: notif,
            message_month: message_month,
        });
        self.send_to_python(call);
        return fut;
    }

    pub fn drop_user(&self, uaid: String) -> MyFuture<DropUserResponse> {
        let (call, fut) = PythonCall::new(&Call::DropUser { uaid });
        self.send_to_python(call);
        return fut;
    }

    pub fn migrate_user(
        &self,
        uaid: String,
        message_month: String,
    ) -> MyFuture<MigrateUserResponse> {
        let (call, fut) = PythonCall::new(&Call::MigrateUser {
            uaid,
            message_month,
        });
        self.send_to_python(call);
        return fut;
    }

    pub fn store_messages(
        &self,
        uaid: String,
        message_month: String,
        mut messages: Vec<protocol::Notification>,
    ) -> MyFuture<StoreMessagesResponse> {
        for message in messages.iter_mut() {
            message.uaid = Some(uaid.clone());
        }
        let (call, fut) = PythonCall::new(&Call::StoreMessages {
            message_month,
            messages,
        });
        self.send_to_python(call);
        return fut;
    }

    fn send_to_python(&self, call: PythonCall) {
        self.tx.send(Some(call)).expect("python went away?");
    }
}

impl PythonCall {
    fn new<T, U>(input: &T) -> (PythonCall, MyFuture<U>)
    where
        T: ser::Serialize,
        U: for<'de> de::Deserialize<'de> + 'static,
    {
        let (tx, rx) = oneshot::channel();
        let call = PythonCall {
            input: serde_json::to_string(input).unwrap(),
            output: Box::new(|json: &str| { drop(tx.send(json_or_error(json))); }),
        };
        let rx = Box::new(rx.then(|res| match res {
            Ok(Ok(s)) => Ok(serde_json::from_str(&s)?),
            Ok(Err(e)) => Err(e),
            Err(_) => Err("call canceled from python".into()),
        }));
        (call, rx)
    }
}

fn json_or_error(json: &str) -> Result<String> {
    if let Ok(err) = serde_json::from_str::<PythonError>(json) {
        if err.error {
            return Err(format!("python exception: {}", err.error_msg).into());
        }
    }
    Ok(json.to_string())
}
