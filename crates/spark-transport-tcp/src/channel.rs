use crate::{
    backpressure::BackpressureState,
    error::{self, map_io_error},
    util::{deadline_expired, deadline_remaining, run_with_context, to_socket_addr},
};
use socket2::SockRef;
use spark_core::{
    context::ExecutionContext,
    contract::CallContext,
    error::CoreError,
    status::ready::{PollReady, ReadyCheck, ReadyState},
    transport::TransportSocketAddr,
};
use std::{
    io::{self, IoSlice},
    net::Shutdown as StdShutdown,
    sync::{Arc, Mutex},
    task::Poll,
};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpStream as TokioTcpStream,
    sync::Mutex as AsyncMutex,
};

#[derive(Debug)]
struct TcpChannelInner {
    stream: AsyncMutex<TokioTcpStream>,
    backpressure: Mutex<BackpressureState>,
    peer_addr: TransportSocketAddr,
    local_addr: TransportSocketAddr,
}

/// TCP 通道的最小实现，封装读写、半关闭与背压探测。
///
/// # 教案式注释
///
/// ## 意图 (Why)
/// - 为上层 Handler 提供对单个 TCP 连接的直接控制，同时贯彻
///   `CallContext` 的取消/超时语义；
/// - 在无须了解 Tokio 具体类型的情况下，完成字节流读写与半关闭。
///
/// ## 逻辑 (How)
/// - 内部以 `tokio::sync::Mutex` 包裹 `TcpStream`，确保多线程调用 `&self`
///   方法时的互斥；
/// - 读写操作通过内部工具函数 `run_with_context` 注入取消
///   与截止时间；
/// - 背压信号由内部 `BackpressureState` 统计 `WouldBlock` 与锁竞争次数，并映射为
///   [`ReadyState`]；
/// - `ShutdownDirection` 封装标准库的半关闭语义，便于调用方显式声明关闭方向。
///
/// ## 契约 (What)
/// - `connect`：根据 `CallContext` 建立到目标地址的连接；
/// - `read`/`write`/`writev`：执行一次 IO 操作，返回实际读写字节数；
/// - `shutdown`：执行半关闭；
/// - `poll_ready`：无阻塞地返回当前写路径的背压状态；
/// - `peer_addr`/`local_addr`：提供结构化的地址元数据。
///
/// ## 注意事项 (Trade-offs)
/// - 由于使用互斥锁序列化读写，无法与 `TcpStream::split` 一样实现真正的
///   全双工；若需要高并发，可在未来引入独立的读/写半部实现；
/// - `poll_ready` 的取消响应通过检查 `ExecutionContext` 的截止与取消标志，
///   若调用频率极低可能感知不及时；
/// - `writev` 目前只执行一次 vectored 写入，如需完全写满需上层循环调用。
#[derive(Clone, Debug)]
pub struct TcpChannel {
    inner: Arc<TcpChannelInner>,
}

impl TcpChannel {
    pub(crate) fn from_parts(
        stream: TokioTcpStream,
        local_addr: TransportSocketAddr,
        peer_addr: TransportSocketAddr,
    ) -> Self {
        Self {
            inner: Arc::new(TcpChannelInner {
                stream: AsyncMutex::new(stream),
                backpressure: Mutex::new(BackpressureState::new()),
                peer_addr,
                local_addr,
            }),
        }
    }

    /// 根据上下文建立到目标地址的连接。
    pub async fn connect(ctx: &CallContext, addr: TransportSocketAddr) -> Result<Self, CoreError> {
        let socket_addr = to_socket_addr(addr);
        let stream =
            run_with_context(ctx, error::CONNECT, TokioTcpStream::connect(socket_addr)).await?;
        let local = stream
            .local_addr()
            .map_err(|err| map_io_error(error::CONNECT, err))?;
        let peer = stream
            .peer_addr()
            .map_err(|err| map_io_error(error::CONNECT, err))?;
        Ok(Self::from_parts(
            stream,
            TransportSocketAddr::from(local),
            TransportSocketAddr::from(peer),
        ))
    }

    /// 读取数据到缓冲区。
    pub async fn read(&self, ctx: &CallContext, buf: &mut [u8]) -> Result<usize, CoreError> {
        run_with_context(ctx, error::READ, async {
            let mut guard = self.inner.stream.lock().await;
            guard.read(buf).await
        })
        .await
    }

    /// 将整个缓冲区写入套接字。
    pub async fn write(&self, ctx: &CallContext, buf: &[u8]) -> Result<usize, CoreError> {
        if buf.is_empty() {
            return Ok(0);
        }
        let len = buf.len();
        let written = run_with_context(ctx, error::WRITE, async {
            let mut guard = self.inner.stream.lock().await;
            guard.write_all(buf).await.map(|_| len)
        })
        .await?;
        if let Ok(mut state) = self.inner.backpressure.lock() {
            state.on_ready();
        }
        Ok(written)
    }

    /// 使用 vectored IO 写入多个缓冲区。仅执行一次写入尝试。
    pub async fn writev(
        &self,
        ctx: &CallContext,
        bufs: &[IoSlice<'_>],
    ) -> Result<usize, CoreError> {
        if bufs.is_empty() {
            return Ok(0);
        }
        let written = run_with_context(ctx, error::WRITE_VECTORED, async {
            let mut guard = self.inner.stream.lock().await;
            guard.write_vectored(bufs).await
        })
        .await?;
        if let Ok(mut state) = self.inner.backpressure.lock() {
            state.on_ready();
        }
        Ok(written)
    }

    /// 根据方向执行半关闭。
    pub async fn shutdown(
        &self,
        ctx: &CallContext,
        direction: ShutdownDirection,
    ) -> Result<(), CoreError> {
        run_with_context(ctx, error::SHUTDOWN, async {
            let mut guard = self.inner.stream.lock().await;
            match direction {
                ShutdownDirection::Write => AsyncWriteExt::shutdown(&mut *guard).await,
                ShutdownDirection::Read => sync_shutdown(&guard, StdShutdown::Read),
                ShutdownDirection::Both => {
                    AsyncWriteExt::shutdown(&mut *guard).await?;
                    sync_shutdown(&guard, StdShutdown::Read)
                }
            }
        })
        .await?;
        if let Ok(mut state) = self.inner.backpressure.lock() {
            state.on_ready();
        }
        Ok(())
    }

    /// 获取对端地址。
    pub fn peer_addr(&self) -> TransportSocketAddr {
        self.inner.peer_addr
    }

    /// 获取本地地址。
    pub fn local_addr(&self) -> TransportSocketAddr {
        self.inner.local_addr
    }

    /// 检查写通道的即时背压状态。
    pub fn poll_ready(&self, ctx: &ExecutionContext<'_>) -> PollReady<CoreError> {
        if deadline_expired(ctx.deadline()) {
            return Poll::Ready(ReadyCheck::Err(error::timeout_error(error::POLL_READY)));
        }
        if ctx.cancellation().is_cancelled() {
            return Poll::Ready(ReadyCheck::Err(error::cancelled_error(error::POLL_READY)));
        }

        if let Some(remaining) = deadline_remaining(ctx.deadline())
            && remaining.is_zero()
        {
            return Poll::Ready(ReadyCheck::Err(error::timeout_error(error::POLL_READY)));
        }

        let mut state = match self.inner.backpressure.lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        };
        state.refresh();

        match self.inner.stream.try_lock() {
            Ok(guard) => match guard.try_write(&[]) {
                Ok(_) => {
                    state.on_ready();
                    Poll::Ready(ReadyCheck::Ready(ReadyState::Ready))
                }
                Err(ref err) if err.kind() == io::ErrorKind::WouldBlock => {
                    let ready_state = state.on_would_block();
                    Poll::Ready(ReadyCheck::Ready(ready_state))
                }
                Err(err) => {
                    drop(guard);
                    let core_error = map_io_error(error::POLL_READY, err);
                    Poll::Ready(ReadyCheck::Err(core_error))
                }
            },
            Err(_) => {
                let ready_state = state.on_manual_busy();
                Poll::Ready(ReadyCheck::Ready(ready_state))
            }
        }
    }
}

/// 表示半关闭的方向。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ShutdownDirection {
    /// 关闭写半部。
    Write,
    /// 关闭读半部。
    Read,
    /// 同时关闭读写半部。
    Both,
}

impl From<ShutdownDirection> for StdShutdown {
    fn from(value: ShutdownDirection) -> Self {
        match value {
            ShutdownDirection::Write => StdShutdown::Write,
            ShutdownDirection::Read => StdShutdown::Read,
            ShutdownDirection::Both => StdShutdown::Both,
        }
    }
}

fn sync_shutdown(stream: &TokioTcpStream, direction: StdShutdown) -> io::Result<()> {
    let sock = SockRef::from(stream);
    sock.shutdown(direction)
}
