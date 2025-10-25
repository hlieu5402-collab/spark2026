#![doc = r#"
# spark-impl-tck

## 设计动机（Why）
- **定位**：承载各类传输实现（TCP/UDP/TLS/QUIC）的兼容性验证，统一运行 Spark Transport Compatibility Kit (TCK)。
- **架构角色**：连接 `spark-core` 的抽象契约与具体 transport crate，确保实现遵循协议、错误语义与运行时假设。
- **设计理念**：通过将 TCK 驱动与实现剥离，支持在 CI 中对多种传输实现进行一致性回归测试。

## 核心契约（What）
- **输入条件**：未来将提供公共入口以注册传输实现、提供上下文与模拟服务；当前仅作为占位模块。
- **输出/保障**：目标是对外暴露统一的测试集合与断言工具，帮助识别协议偏差；占位阶段尚未导出 API。
- **前置约束**：依赖 transport crate 与 `spark-core` 的基础设施，运行场景默认处于 Tokio 等异步运行时之上。

## 实现策略（How）
- **依赖治理**：以 dev-dependency 的形式引用各 transport crate，确保 TCK 在编译期即可校验接口可用性，同时避免生产代码直接耦合具体实现。
- **扩展点**：未来可根据不同运行时引入额外的测试后端，或通过特性选择要加载的传输集合。
- **错误策略**：将沿用 `spark-core` 的错误抽象，并结合 `thiserror` 等工具提供易读的断言信息（后续补齐）。

## 风险与考量（Trade-offs）
- **编译成本**：引用多个 transport 实现会增加编译时间；通过 dev-dependency 降低发布产物的体积影响。
- **功能缺口**：当前无具体测试逻辑，需在后续版本补充案例与驱动脚手架。
"#]

pub(crate) mod placeholder {}

#[cfg(test)]
mod transport {
    use spark_core::{contract::CallContext, transport::TransportSocketAddr};
    use spark_transport_tcp::{ShutdownDirection, TcpChannel, TcpListener, TcpSocketConfig};
    use std::{net::SocketAddr, time::Duration};
    use tokio::time::sleep;

    /// 验证 TCP 通道在优雅关闭时遵循“FIN→等待 EOF→释放”的顺序，并正确应用 `linger` 配置。
    ///
    /// # 教案级注释
    ///
    /// ## 意图（Why）
    /// - 确保 `TcpChannel::close_graceful` 能在调用 `shutdown(Write)` 后等待对端发送 FIN，
    ///   与契约文档一致；
    /// - 校验 `TcpSocketConfig::with_linger(Some(..))` 在建连阶段确实生效，避免在生产
    ///   环境中因配置缺失导致 RST 行为不确定。
    ///
    /// ## 体系位置（Architecture）
    /// - 测试位于实现 TCK 的 crate 中，模拟“客户端主动关闭、服务端稍后响应 FIN”场景，
    ///   作为传输层的守门测试；
    /// - 运行时依赖 Tokio 多线程执行器，以贴近真实部署环境。
    ///
    /// ## 核心逻辑（How）
    /// - 启动监听器并接受连接；
    /// - 客户端使用自定义 `linger` 建立连接并调用 `close_graceful`；
    /// - 服务端在读取到 EOF 后延迟一段时间再关闭写半部，
    ///   断言客户端在此期间保持挂起；
    /// - 最终断言 `close_graceful` 成功完成且 `linger` 读取结果与配置一致。
    ///
    /// ## 契约（What）
    /// - **前置条件**：测试环境允许绑定环回地址并启动 Tokio 运行时；
    /// - **后置条件**：若任何步骤违反契约，测试将 panic，从而阻止回归通过。
    ///
    /// ## 注意事项（Trade-offs）
    /// - `sleep(150ms)` 模拟服务端清理资源的耗时，实测中可根据环境调整；
    /// - `SO_LINGER` 在 Linux 上以秒为单位，此处选择 1 秒避免取整误差。
    #[tokio::test(flavor = "multi_thread")]
    async fn tcp_graceful_half_close() {
        let bind_addr: SocketAddr = "127.0.0.1:0".parse().expect("invalid bind addr");
        let listener = TcpListener::bind(TransportSocketAddr::from(bind_addr))
            .await
            .expect("bind listener");
        let local_addr = listener.local_addr();

        let server_ctx = CallContext::builder().build();
        let server_task_ctx = server_ctx.clone();

        let server_task = tokio::spawn(async move {
            let (server_channel, _) = listener
                .accept(&server_task_ctx)
                .await
                .expect("accept connection");
            let mut sink = [0u8; 1];
            let bytes = server_channel
                .read(&server_task_ctx, &mut sink)
                .await
                .expect("read client FIN");
            assert_eq!(bytes, 0, "server must observe client FIN");
            sleep(Duration::from_millis(150)).await;
            server_channel
                .shutdown(&server_task_ctx, ShutdownDirection::Write)
                .await
                .expect("shutdown write half");
        });

        let client_ctx = CallContext::builder().build();
        let client_config = TcpSocketConfig::new().with_linger(Some(Duration::from_secs(1)));
        let client_channel =
            TcpChannel::connect_with_config(&client_ctx, local_addr, client_config.clone())
                .await
                .expect("connect client");

        assert_eq!(
            client_channel.linger().await.expect("query linger option"),
            Some(Duration::from_secs(1))
        );
        assert_eq!(
            client_channel.config().linger(),
            client_config.linger(),
            "config cache should match applied linger",
        );

        let close_ctx = client_ctx.clone();
        let closing_channel = client_channel.clone();
        let close_task =
            tokio::spawn(async move { closing_channel.close_graceful(&close_ctx).await });

        sleep(Duration::from_millis(50)).await;
        assert!(
            !close_task.is_finished(),
            "close_graceful must wait for peer EOF",
        );

        server_task.await.expect("server task join");

        close_task
            .await
            .expect("close task join")
            .expect("close graceful result");

        drop(client_channel);
    }
}
