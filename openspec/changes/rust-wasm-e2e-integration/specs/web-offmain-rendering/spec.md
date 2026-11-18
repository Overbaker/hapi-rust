## ADDED Requirements

### Requirement: 大 Diff 离开主线程
DiffView 对小于阈值（<500 行且 <16 KB）的 diff MUST 保持同步渲染；达到阈值的 diff MUST 在 Worker 中计算，主线程 MUST NOT 出现 >50 ms 长任务，且渲染结果 MUST 保留现有双列行号语义。

#### Scenario: 逐行重建
- **WHEN** 对任意字符串对 (A,B) 渲染 diff
- **THEN** 由 equal+del 重建 A、由 equal+add 重建 B，行号单调递增，且与 `DiffView` 既有测试一致

#### Scenario: 过期结果
- **WHEN** 较早发起的 Worker 请求晚于新请求返回
- **THEN** 其结果 MUST 被丢弃，不覆盖较新渲染

### Requirement: 流式 Markdown 尾块增量
流式渲染 MUST 只重新解析活动尾块，已稳定块 MUST 复用；渲染结果 MUST 与全量渲染等价（fence 边界、数学、Mermaid、HAPI 自定义链接/插件不变）。WASM 仅在 JS 优化（尾块 memo + rAF 合并）后 p95 帧时仍 >12 ms 时才启用。

#### Scenario: 任意切分等价
- **WHEN** 同一文档按任意 UTF-8 码点边界切成累积分块依次渲染
- **THEN** 终态 DOM 等于一次性全量渲染的 DOM

### Requirement: 页面水合不阻塞主线程
会话切换/向前翻页的 JSON 解析与规范化在主线程耗时 p95 ≥25 ms 时，MUST 移入 Worker；`shared/fixtures/**` MUST 无漂移。

#### Scenario: 超阈值移入 Worker
- **WHEN** 主线程 parse+normalize p95 ≥25 ms
- **THEN** 该解析在 Worker 中完成，且 `shared/fixtures/**` 无漂移

### Requirement: 零回归回落
Worker 不可用、wasm 404/MIME 错误或 feature flag 关闭时，系统 MUST 回落到 JS 路径，既有 web 测试全部通过；任何 WASM HTML MUST 经净化，不得产生 script/iframe/object/embed/事件属性。

#### Scenario: WASM 不可用
- **WHEN** Worker 不可用、wasm 404/MIME 错误或 flag 关闭
- **THEN** 回落 JS 路径且既有 web 测试全部通过

#### Scenario: 净化
- **WHEN** 输入含 script/iframe/object/embed/事件属性
- **THEN** 输出 DOM 不含这些元素或属性
