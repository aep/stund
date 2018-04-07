// Copyright 2018 Peter Williams <peter@newton.cx>
// Licensed under the MIT License.

//! Interfacing with the daemon.

use failure::{Error, ResultExt};
use futures::{Async, AsyncSink, Future, Poll, Sink, Stream};
use futures::sink::Send;
use libc;
use state_machine_future::RentToOwn;
use std::env;
use std::io;
use std::mem;
use std::process;
use std::thread;
use std::time;
use std::os::unix::io::AsRawFd;
use tokio_core::reactor::Core;
use tokio_io::AsyncRead;
use tokio_io::codec::length_delimited::{FramedRead, FramedWrite};
use tokio_io::io::{ReadHalf, WriteHalf};
use tokio_serde_json::{ReadJson, WriteJson};
use tokio_uds::UnixStream;

use super::*;


type Ser = WriteJson<FramedWrite<WriteHalf<UnixStream>>, ClientMessage>;
type De = ReadJson<FramedRead<ReadHalf<UnixStream>>, ServerMessage>;
type UserInputStream = Box<Stream<Item = Vec<u8>, Error = io::Error>>;
type UserOutputSink = Box<Sink<SinkItem = Vec<u8>, SinkError = io::Error>>;


pub struct Connection {
    core: Core,
    ser: Ser,
    de: De,
}

impl Connection {
    pub fn establish(autolaunch: bool) -> Result<Self, Error> {
        let core = Core::new().context("couldn't create IO core?")?;
        let handle = core.handle();
        let sock_path = get_socket_path().context("couldn't get path to talk to daemon")?;

        let conn = match UnixStream::connect(&sock_path, &handle) {
            Ok(c) => c,
            Err(e) => {
                if !autolaunch {
                    return Err(e.into());
                }

                let curr_exe = env::current_exe().context("couldn't get current executable path")?;

                let status = process::Command::new(&curr_exe)
                    .arg("daemon")
                    .status()
                    .context("daemon launcher reported failure")?;

                thread::sleep(time::Duration::from_millis(300));

                if status.success() {
                    UnixStream::connect(&sock_path, &handle)
                        .context("failed to connect to daemon even after launching it")?
                } else {
                    return Err(format_err!("failed to launch background daemon"));
                }
            },
        };

        unsafe {
            // Without turning on linger, I find that the tokio-ized version
            // loses the last bytes of the session. Let's just ignore the
            // return value of setsockopt(), though.
            let linger = libc::linger { l_onoff: 1, l_linger: 2 };
            libc::setsockopt(conn.as_raw_fd(), libc::SOL_SOCKET, libc::SO_LINGER,
                             (&linger as *const libc::linger) as _,
                             mem::size_of::<libc::linger>() as libc::socklen_t);
        }

        let (read, write) = conn.split();
        let wdelim = FramedWrite::new(write);
        let ser = WriteJson::new(wdelim);
        let rdelim = FramedRead::new(read);
        let de = ReadJson::new(rdelim);

        Ok(Connection {
            core: core,
            ser: ser,
            de: de,
        })
    }


    pub fn close(mut self) -> Result<(), Error> {
        self.core.run(self.ser.send(ClientMessage::Goodbye))?;
        Ok(())
    }


    pub fn send_open<T, R>(
        mut self, params: OpenParameters, tx_user: T, rx_user: R
    ) -> Result<(OpenResult, Self), Error>
        where T: 'static + Sink<SinkItem = Vec<u8>, SinkError = io::Error>,
              R: 'static + Stream<Item = Vec<u8>, Error = io::Error>
    {
        let fut = self.ser.send(ClientMessage::Open(params));
        let wf = OpenWorkflow::start(fut, self.de, Box::new(tx_user), Box::new(rx_user));
        let (ser, de, result) = self.core.run(wf)?;
        self.ser = ser;
        self.de = de;
        Ok((result, self))
    }
}


#[derive(StateMachineFuture)]
#[allow(unused)] // get lots of these spuriously; custom derive stuff?
enum OpenWorkflow {
    #[state_machine_future(start, transitions(FirstAck))]
    Issue {
        tx_ssh: Send<Ser>,
        rx_ssh: De,
        tx_user: UserOutputSink,
        rx_user: UserInputStream,
    },

    #[state_machine_future(transitions(Finished, Communicating))]
    FirstAck {
        tx_ssh: Ser,
        rx_ssh: De,
        tx_user: UserOutputSink,
        rx_user: UserInputStream,
        saw_ok: bool,
    },

    #[state_machine_future(transitions(CleaningUpIo))]
    Communicating {
        tx_ssh: Ser,
        rx_ssh: De,
        ssh_buf: Vec<u8>,
        tx_user: UserOutputSink,
        rx_user: UserInputStream,
        user_buf: Vec<u8>,
        finished: FinishCommunicationState,
    },

    #[state_machine_future(transitions(CleaningUpIo, Finished))]
    CleaningUpIo {
        tx_ssh: Ser,
        rx_ssh: De,
        sent_finished_message: bool,
        saw_ok: bool,
    },

    #[state_machine_future(ready)]
    Finished((Ser, De, OpenResult)),

    #[state_machine_future(error)]
    Failed(Error),
}


#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum FinishCommunicationState {
    NoLeads,
    SawFirstEnter,
    SawPeriod,
    SawSecondEnter,
}

impl FinishCommunicationState {
    pub fn transition(&self, byte: u8) -> Self {
        match *self {
            FinishCommunicationState::NoLeads => {
                if byte == 0x0A {
                    return FinishCommunicationState::SawFirstEnter;
                }
            },

            FinishCommunicationState::SawFirstEnter => {
                if byte == 0x2E {
                    return FinishCommunicationState::SawPeriod;
                }
            },

            FinishCommunicationState::SawPeriod => {
                if byte == 0x0A {
                    return FinishCommunicationState::SawSecondEnter;
                }
            },

            FinishCommunicationState::SawSecondEnter => {
                return FinishCommunicationState::SawSecondEnter;
            },
        }

        FinishCommunicationState::NoLeads
    }
}


impl PollOpenWorkflow for OpenWorkflow {
    fn poll_issue<'a>(
        state: &'a mut RentToOwn<'a, Issue>
    ) -> Poll<AfterIssue, Error> {
        let ser = try_ready!(state.tx_ssh.poll());

        let state = state.take();
        transition!(FirstAck {
            tx_ssh: ser,
            rx_ssh: state.rx_ssh,
            tx_user: state.tx_user,
            rx_user: state.rx_user,
            saw_ok: false,
        })
    }

    fn poll_first_ack<'a>(
        state: &'a mut RentToOwn<'a, FirstAck>
    ) -> Poll<AfterFirstAck, Error> {
        while let Async::Ready(msg) = state.rx_ssh.poll()? {
            match msg {
                Some(ServerMessage::Ok) => {
                    state.saw_ok = true;
                },

                Some(ServerMessage::Error(text)) => {
                    return Err(format_err!("{}", text));
                },

                Some(ServerMessage::TunnelAlreadyOpen) => {
                    let state = state.take();
                    transition!(Finished((state.tx_ssh, state.rx_ssh, OpenResult::AlreadyOpen)));
                },

                Some(other) => {
                    return Err(format_err!("unexpected response from daemon: {:?}", other));
                },

                None => {
                    return Err(format_err!("connection closed (?)"));
                },
            }
        }

        if state.saw_ok {
            let state = state.take();

            transition!(Communicating {
                rx_user: state.rx_user,
                tx_user: state.tx_user,
                user_buf: Vec::new(),
                finished: FinishCommunicationState::SawFirstEnter,
                tx_ssh: state.tx_ssh,
                rx_ssh: state.rx_ssh,
                ssh_buf: Vec::new(),
            })
        }

        Ok(Async::NotReady)
    }

    fn poll_communicating<'a>(
        state: &'a mut RentToOwn<'a, Communicating>
    ) -> Poll<AfterCommunicating, Error> {
        // New text from the daemon?

        while let Async::Ready(msg) = state.rx_ssh.poll()? {
            match msg {
                Some(ServerMessage::SshData(data)) => {
                    state.user_buf.extend_from_slice(&data);
                },

                Some(ServerMessage::Error(e)) => {
                    return Err(format_err!("{}", e));
                }

                Some(other) => {
                    return Err(format_err!("unexpected message from the daemon: {:?}", other));
                },

                None => {},
            }
        }

        // New text from the user?

        while let Async::Ready(bytes) = state.rx_user.poll()? {
            match bytes {
                None => {
                    return Err(format_err!("EOF on terminal (?)"));
                },

                Some(b) => {
                    state.ssh_buf.extend_from_slice(&b);

                    for single_byte in &b {
                        state.finished = state.finished.transition(*single_byte);
                    }
                }
            }
        }

        // Ready/able to send bytes to the user?

        if state.user_buf.len() != 0 {
            let buf = state.user_buf.clone();

            if let AsyncSink::Ready = state.tx_user.start_send(buf)? {
                    state.user_buf.clear();
            }
        }

        // Ready/able to send bytes to the daemon?

        if state.ssh_buf.len() != 0 {
            let buf = state.ssh_buf.clone();

            if let AsyncSink::Ready = state.tx_ssh.start_send(ClientMessage::UserData(buf))? {
                state.ssh_buf.clear();
            }
        }

        // Gotta flush those transmissions.

        try_ready!(state.tx_user.poll_complete());
        try_ready!(state.tx_ssh.poll_complete());

        // Next step?

        if let FinishCommunicationState::SawSecondEnter = state.finished {
            let mut state = state.take();
            transition!(CleaningUpIo {
                tx_ssh: state.tx_ssh,
                rx_ssh: state.rx_ssh,
                sent_finished_message: false,
                saw_ok: false,
            })
        }

        Ok(Async::NotReady)
    }

    fn poll_cleaning_up_io<'a>(
        state: &'a mut RentToOwn<'a, CleaningUpIo>
    ) -> Poll<AfterCleaningUpIo, Error> {
        if !state.sent_finished_message {
            if let AsyncSink::Ready = state.tx_ssh.start_send(ClientMessage::EndOfUserData)? {
                state.sent_finished_message = true;
            }
        }

        try_ready!(state.tx_ssh.poll_complete());

        while let Async::Ready(msg) = state.rx_ssh.poll()? {
            match msg {
                Some(ServerMessage::SshData(_data)) => {
                    eprintln!("warning: ignored some trailing SSH output");
                },

                Some(ServerMessage::Error(e)) => {
                    return Err(format_err!("{}", e));
                }

                Some(ServerMessage::Ok) => {
                    state.saw_ok = true;
                }

                Some(other) => {
                    return Err(format_err!("unexpected message from the daemon: {:?}", other));
                },

                None => {},
            }
        }

        // What's next?

        if state.saw_ok {
            let state = state.take();
            transition!(Finished((state.tx_ssh, state.rx_ssh, OpenResult::Success)))
        }

        Ok(Async::NotReady)
    }
}
