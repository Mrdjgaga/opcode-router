# opcode-router

`opcode-router` 是一个面向 Rust 服务端的数字 opcode 路由库。它把显式标记的同步或异步处理器转换为类型安全的分发代码，适合游戏服务器、网关和自定义二进制协议。

它只负责“根据 opcode 调用处理器”，不负责网络连接、消息队列、Actor、序列化或数据库。

## 功能

- 只路由带 `#[route(id = ...)]` 的方法，普通内部方法不会意外暴露。
- 直接模式生成常量 `match`，没有运行时注册和哈希查找。
- 处理器可返回 `()` 或 `CallResult`，错误不会被静默丢弃。
- 编译期检查缺失 ID、非法 ID、同一静态路由中的重复 ID 和不一致参数。
- 可选的 `linkme` 模式可以收集同一类型分散在多个 `impl` 中的路由。
- 保留旧名称 `reflect_methods`、`callable` 和 `define_static_dispatch`，方便现有项目逐步迁移。

## 直接路由

一个 `impl` 内的路由可以直接使用 `#[route_methods]`：

```rust
use opcode_router::{CallResult, route_methods};

struct Player {
    score: u32,
}

#[route_methods(id_type = u16)]
impl Player {
    #[route(id = 2001u16)]
    async fn add_score(&mut self, value: u32) -> CallResult {
        self.score += value;
        Ok(())
    }

    // 没有 #[route]，不会进入路由表。
    fn internal_only(&self) {}
}

# async fn example() -> CallResult {
let mut player = Player { score: 0 };
player.call_by_id(2001u16, 10).await?;
# Ok(())
# }
```

同一路由组中的处理器必须使用相同的参数类型。生成的方法也支持按名称调用：`call_by_name("add_score", 10).await`。

如果同一类型的处理器散落在多个源文件，可以在各 `impl` 上使用 `#[route_methods(collect_only)]`，然后在类型所属 crate 中生成一个统一的直接分发表：

```rust,ignore
use opcode_router::define_static_router;

define_static_router! {
    target = PlayerActor,
    id_type = u16,
    scan = "src/**/*.rs",
}
```

该模式会在编译期读取匹配的 Rust 源文件，适合单一应用仓库和追求最低分发开销的场景。

## 多 `impl` 静态注册

如果不希望编译宏扫描源文件，也可以通过 `linkme` 收集同一类型的多个 `impl`。首先声明路由表：

```rust,ignore
use opcode_router::declare_distributed_router;

pub struct GameSession;
pub enum HandlerError { Rejected }

declare_distributed_router! {
    pub static GAME_ROUTES;
    entry = GameRoute;
    target = GameSession;
    id = u16;
    args = (Vec<u8>,);
    error = HandlerError;
}
```

然后在这个类型的不同 `impl` 上注册方法：

```rust,ignore
use opcode_router::distributed_routes;

#[distributed_routes(registry = GAME_ROUTES, entry = GameRoute)]
impl GameSession {
    #[route(id = 2001u16)]
    async fn handle_login(&mut self, body: Vec<u8>) -> Result<(), HandlerError> {
        Ok(())
    }
}
```

最终程序必须依赖并链接插件 crate。启动时构建一次路由器；它会建立只读索引并拒绝重复 opcode：

```rust,ignore
let router = opcode_router::DistributedRouter::new(&GAME_ROUTES)?;
router.dispatch(&mut session, opcode, (body,)).await?;
```

这个模式仍然以 `impl` 方法作为处理器。由于 Rust 不允许外部 crate 给宿主类型新增 inherent method，因此它不承诺跨 crate 插件扩展。

## 选择哪种模式

- 处理器都在应用 crate 内：优先直接路由，生成的 `match` 最简单、开销最低。
- 不想扫描源文件，但需要收集多个 `impl`：使用 `linkme` 路由；它多一次启动期建表和运行时哈希查找。

## License

Licensed under the MIT License.
