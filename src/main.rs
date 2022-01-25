use clap::Parser;
use inotify::*;
use log::*;
use std::{
    convert::TryFrom,
    env::*,
    fs::File,
    net::SocketAddr,
    os::unix::{io::AsRawFd, prelude::RawFd},
    path::PathBuf,
    sync::{
        atomic::{AtomicU64, Ordering},
        Mutex,
    },
};
use tokio::io::{unix::AsyncFd, AsyncBufReadExt, BufReader};
use tokio::net::TcpStream;
use tokio::sync::watch;

#[derive(Parser)]
struct Opts {
    /// The file which will be broadcast to all clients
    path: PathBuf,
    /// The port number on which to listen for new connections
    #[clap(long, short)]
    port: u16,
}

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt::init();
    match main_2(Opts::parse()).await {
        Ok(()) => (),
        Err(e) => {
            eprintln!("{}", e);
            std::process::exit(1);
        }
    };
}

type Result<T, E = Box<dyn std::error::Error>> = std::result::Result<T, E>;

pub static FILE_LENGTH: AtomicU64 = AtomicU64::new(0);

async fn main_2(opts: Opts) -> Result<()> {
    loggerv::init_with_level(LogLevel::Info)?;

    let file = File::open(&opts.path)?;
    let file_fd = file.as_raw_fd();
    let file_len = file.metadata()?.len();
    FILE_LENGTH.store(file_len, Ordering::SeqCst);
    let (tx, rx) = watch::channel::<()>(());
    let mut inotify = Inotify::init()?;
    inotify
        .add_watch(
            &opts.path,
            WatchMask::MODIFY | WatchMask::DELETE_SELF | WatchMask::MOVE_SELF,
        )
        ?;

    {
        // Start the file-watching task
        let inotify_fd = AsyncFd::new(inotify.as_raw_fd())?;
        let mut inotify_buf = vec![0; 4096];
        tokio::task::spawn(async move {
            loop {
                let mut guard = inotify_fd.readable().await.unwrap();
                for ev in inotify.read_events(&mut inotify_buf).unwrap() {
                    if ev.mask.contains(EventMask::MODIFY) {
                        let file_len = file.metadata().unwrap().len();
                        info!("{:?}: File length is now {}", ev.wd, file_len);
                        FILE_LENGTH.store(file_len, Ordering::SeqCst);
                        tx.send(()).unwrap();
                    } else if ev.mask.contains(EventMask::DELETE_SELF)
                        || ev.mask.contains(EventMask::MOVE_SELF)
                    {
                        info!("Watched file disappeared");
                        std::process::exit(0);
                    }
                }
                guard.clear_ready();
            }
        });
    }

    let listen_addr = SocketAddr::new([0, 0, 0, 0].into(), opts.port);
    let listener = tokio::net::TcpListener::bind(&listen_addr)
        .await
        .expect("Bind listen sock");
    info!(
        "Serving files from {} on {}",
        current_dir()?.display(),
        listen_addr
    );
    loop {
        let (sock, addr) = listener.accept().await?;
        info!("{}: New client connected", addr);
        tokio::task::spawn(handle_client(sock, file_fd, rx.clone()));
    }
}

async fn handle_client(
    mut sock: TcpStream,
    fd: RawFd,
    mut rx: watch::Receiver<()>,
) -> Result<()> {
    // The first thing the client will do is send a header
    // TODO: timeout
    // TODO: length limit
    let mut buf = String::new();
    tokio::io::BufReader::new(sock).read_line(&mut buf).await?;
    info!("Client sent header bytes {:?}", &buf);
    let idx = buf.as_str().trim().parse()?;
    info!("Client sent header {:?}", idx);
    let initial_offset = if header >= 0 {
        Ok(header)
    } else {
        let cur_len = i64::try_from(FILE_LENGTH.load(Ordering::SeqCst))?;
        Ok(cur_len - header.neg())
    };

    let mut offset = initial_offset;
    loop {
        sock.writable().await?;
        info!("Socket has become writable");
        // How many bytes the client wants
        let file_len = FILE_LENGTH.load(Ordering::SeqCst);
        let wanted = i64::try_from(file_len)? - offset;
        if wanted <= 0 {
            // We're all caught-up.  Wait for new data to be written
            // to the file before continuing.
            info!("Waiting for changes");
            match rx.changed().await {
                Ok(()) => continue,
                Err(_) => {
                    // The sender is gone.  This means that the file has
                    // been deleted.
                    info!("Closing socket: file was deleted");
                    return Ok(());
                }
            }
        }

        /// The maximum number of bytes which will be `sendfile()`'d to a client before moving onto the
        /// next waiting client.
        ///
        /// A bigger size increases total throughput, but may allow a client who is reading a lot of data
        /// to hurt reaction latency for other clients.
        const CHUNK_SIZE: i64 = 1024 * 1024;
        // How many bytes the client will get
        let cnt = usize::try_from(wanted.min(CHUNK_SIZE))?;

        info!("Sending {} bytes from offset {}", cnt, offset);
        let ret = sock.try_io(tokio::io::Interest::WRITABLE, || {
            nix::sys::sendfile::sendfile(sock.as_raw_fd(), fd, Some(&mut offset), cnt)
                .map_err(std::io::Error::from)
        });
        if let Err(e) = ret {
            match e.kind() {
                std::io::ErrorKind::BrokenPipe | std::io::ErrorKind::ConnectionReset => {
                    // The client hung up
                    info!("Socket closed by other side");
                    return Ok(());
                }
                std::io::ErrorKind::WouldBlock => {
                    // The socket is not writeable. Wait for it to become writable
                    // again before continuing.
                }
                _ => panic!("{}", e),
            }
        }
    }
}
