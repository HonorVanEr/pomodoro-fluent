//! 窗口几何：迷你悬浮、贴边吸附/收起的纯计算。
//!
//! 这里**不碰窗口**，只做算术 —— 多屏、DPI 缩放、越界这些边界情况才能用单元测试
//! 钉住。窗口的读写在各调用点（`window.rs`）。
//!
//! 所有数值与规则来自 Electron 版 `main.js`（M1 要求行为等价，不要求写法同构）：
//! 迷你模式收起锚点、贴边收起的细条尺寸、吸附触发距离、越过边缘的例外……
//! **改这里之前先去看 `main.js` 对应段落**（`git show electron-archive:main.js`），别凭感觉调。
//!
//! # 单位：为什么要有 [`Metrics`]
//!
//! Electron 的 `getBounds()` / `setBounds()` / `screen.getCursorScreenPoint()` 全是
//! **DIP（逻辑像素）**，所以 `main.js` 里的 `360` / `176` / `64` 这些数字是 DIP。
//! Tauri 这边相反：`outer_size()`、`outer_position()`、`Monitor::work_area()`、
//! `cursor_position()` 全是**物理像素**。
//!
//! 如果直接拿 DIP 的常量去物理空间里算，125% 缩放下窗口会比 Electron 版**小一圈**。
//! 所以这里的做法是：常量按 DIP 定义（[`Metrics`] 里的 `*_DIP`），运行时按所在屏的
//! 缩放比换成物理像素，**之后所有坐标都在物理空间里算**。
//! 这样做的好处是不用把物理坐标来回换算成 DIP —— 少一次取整，也就少一次位置漂移。

/// 完整模式默认尺寸（DIP）
pub const FULL_W_DIP: i32 = 360;
pub const FULL_H_DIP: i32 = 560;
/// 完整模式最小尺寸（DIP）
pub const FULL_MIN_W_DIP: i32 = 300;
pub const FULL_MIN_H_DIP: i32 = 460;

/// 迷你悬浮窗尺寸（DIP）。静置只放时间+文字，悬停文字换成按钮。
pub const MINI_W_DIP: i32 = 176;
pub const MINI_H_DIP: i32 = 64;

/// 贴边收起后细条的长边（DIP；6px 短边由 CSS 绘制）
pub const DOCK_LEN_DIP: i32 = 76;
/// 贴边收起时窗口自身尺寸（DIP）：比可见细条大一圈，留出抓取与命中余量
pub const DOCK_PAD_W_DIP: i32 = 34;
pub const DOCK_PAD_H_DIP: i32 = 40;

/// 松手时距屏幕边缘多近触发吸附（DIP）
pub const DOCK_SNAP_DIST_DIP: i32 = 64;
/// 越过屏幕边缘多深就不再算吸附（DIP，双屏接缝场景）
pub const DOCK_SNAP_INSET_DIP: i32 = 8;

/// 主窗口首次出现时距工作区边缘的留白（DIP）
pub const INIT_MARGIN_DIP: i32 = 24;

/// 拖拽时判定「光标已深入另一块屏」的余量（DIP）
pub const DRAG_SWITCH_MARGIN_DIP: i32 = 40;

/// 滑出前校验光标是否落在细条附近时的外扩余量（**物理像素**，故意不随缩放走：
/// 它本身就是为了吸收 DPI 取整误差，跟着缩放就失去意义了）
pub const DOCK_REVEAL_PAD: i32 = 8;
/// 延时收回前校验光标位置的外扩余量（物理像素，同上）
pub const DOCK_HIDE_PAD: i32 = 2;

/// 设计尺寸按屏幕缩放比换算出的物理像素值。
///
/// 一次几何计算内部**只用一个 `Metrics`**（取当前所在屏的缩放比），
/// 免得同一帧里混用两种缩放。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Metrics {
    pub full_w: i32,
    pub full_h: i32,
    pub full_min_w: i32,
    pub full_min_h: i32,
    pub mini_w: i32,
    pub mini_h: i32,
    pub dock_len: i32,
    pub dock_pad_w: i32,
    pub dock_pad_h: i32,
    pub snap_dist: i32,
    pub snap_inset: i32,
    pub init_margin: i32,
    pub drag_switch_margin: i32,
}

impl Metrics {
    /// 按缩放比把 DIP 设计尺寸换成物理像素。
    ///
    /// 缩放比异常（`0` / `NaN` / 负数）时退回 1.0 —— 宁愿尺寸差一点，也不能让整条
    /// 几何计算变成 `NaN`（`NaN` 转 `i32` 在 Rust 里是 0，窗口会直接跳到左上角）。
    pub fn for_scale(scale: f64) -> Self {
        let s = if scale.is_finite() && scale > 0.0 { scale } else { 1.0 };
        let px = |dip: i32| ((dip as f64) * s).round() as i32;
        // 至少 1px：缩放比极小时不能退化成 0 宽/高的窗口
        let px1 = |dip: i32| px(dip).max(1);
        Self {
            full_w: px1(FULL_W_DIP),
            full_h: px1(FULL_H_DIP),
            full_min_w: px1(FULL_MIN_W_DIP),
            full_min_h: px1(FULL_MIN_H_DIP),
            mini_w: px1(MINI_W_DIP),
            mini_h: px1(MINI_H_DIP),
            dock_len: px1(DOCK_LEN_DIP),
            dock_pad_w: px1(DOCK_PAD_W_DIP),
            dock_pad_h: px1(DOCK_PAD_H_DIP),
            snap_dist: px(DOCK_SNAP_DIST_DIP),
            snap_inset: px(DOCK_SNAP_INSET_DIP),
            init_margin: px(INIT_MARGIN_DIP),
            drag_switch_margin: px(DRAG_SWITCH_MARGIN_DIP),
        }
    }
}

/// 物理像素矩形（窗口位置/尺寸、屏幕工作区都用它）
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Rect {
    pub x: i32,
    pub y: i32,
    pub w: i32,
    pub h: i32,
}

impl Rect {
    pub const fn new(x: i32, y: i32, w: i32, h: i32) -> Self {
        Self { x, y, w, h }
    }

    pub fn right(&self) -> i32 {
        self.x + self.w
    }

    pub fn bottom(&self) -> i32 {
        self.y + self.h
    }

    /// 点是否落在这个矩形内（左闭右开，与 Windows 的窗口包含判定一致）
    pub fn contains(&self, x: i32, y: i32) -> bool {
        x >= self.x && x < self.right() && y >= self.y && y < self.bottom()
    }
}

/// 贴边的边
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Edge {
    Left,
    Right,
    Top,
    Bottom,
}

impl Edge {
    /// 推给渲染层的字符串（渲染层按 `dock-left` / `dock-right` … 拼 body 类名）
    pub fn as_str(self) -> &'static str {
        match self {
            Edge::Left => "left",
            Edge::Right => "right",
            Edge::Top => "top",
            Edge::Bottom => "bottom",
        }
    }
}

/// 等价于 JS 的 `Math.max(lo, Math.min(v, hi))`。
///
/// 注意 `hi < lo` 时 JS 返回 `lo`（窗口比工作区还大时会出现），这里保持一致，
/// 否则大尺寸窗口会被算到工作区外面去。
fn clamp(v: i32, lo: i32, hi: i32) -> i32 {
    if hi < lo {
        lo
    } else {
        v.clamp(lo, hi)
    }
}

/// 主窗口初始摆放：工作区右下角，留 [`Metrics::init_margin`]。
///
/// 与 Electron 版一样**不做限位**：工作区比窗口还小时宁可露出屏幕一部分，
/// 也比把窗口挤到不可见处强。
pub fn initial_position(wa: Rect, w: i32, h: i32, m: &Metrics) -> (i32, i32) {
    (wa.x + wa.w - w - m.init_margin, wa.y + wa.h - h - m.init_margin)
}

/// 进入迷你模式：以原窗口**右上角**为锚点收缩（小窗落在标题栏图钉下方）。
pub fn mini_from_full(full: Rect, m: &Metrics) -> Rect {
    Rect::new(full.x + full.w - m.mini_w, full.y, m.mini_w, m.mini_h)
}

/// 退出迷你模式：以小窗右上角为锚点展开回完整尺寸，并限位在工作区内。
pub fn mini_to_full(cur: Rect, full_w: i32, full_h: i32, wa: Rect) -> Rect {
    let x = clamp(cur.x + cur.w - full_w, wa.x, wa.x + wa.w - full_w);
    let y = clamp(cur.y, wa.y, wa.y + wa.h - full_h);
    Rect::new(x, y, full_w, full_h)
}

/// 贴边状态下的窗口摆放。`hidden = true` 是收起（只露细条），`false` 是滑出的迷你小窗。
///
/// 沿边方向的限位用的是**当前形态**的长边：收起态按 `dock_len`，
/// 展开态按 `mini_h` / `mini_w`。这一条容易写错，写反了收起时会顶出屏幕。
pub fn dock_geometry(edge: Edge, b: Rect, wa: Rect, hidden: bool, m: &Metrics) -> Rect {
    let y = clamp(b.y, wa.y, wa.y + wa.h - if hidden { m.dock_len } else { m.mini_h });
    let x = clamp(b.x, wa.x, wa.x + wa.w - if hidden { m.dock_len } else { m.mini_w });
    if hidden {
        match edge {
            Edge::Left => Rect::new(wa.x, y, m.dock_pad_w, m.dock_len),
            Edge::Right => Rect::new(wa.x + wa.w - m.dock_pad_w, y, m.dock_pad_w, m.dock_len),
            Edge::Top => Rect::new(x, wa.y, m.dock_len, m.dock_pad_h),
            Edge::Bottom => Rect::new(x, wa.y + wa.h - m.dock_pad_h, m.dock_len, m.dock_pad_h),
        }
    } else {
        match edge {
            Edge::Left => Rect::new(wa.x, y, m.mini_w, m.mini_h),
            Edge::Right => Rect::new(wa.x + wa.w - m.mini_w, y, m.mini_w, m.mini_h),
            Edge::Top => Rect::new(x, wa.y, m.mini_w, m.mini_h),
            Edge::Bottom => Rect::new(x, wa.y + wa.h - m.mini_h, m.mini_w, m.mini_h),
        }
    }
}

/// 松手时判定吸附到哪条边。
///
/// 判定式照搬 `main.js`：左边比的是 `wa.x - b.x`、右边比的是 `b.right - wa.right()`，
/// 也就是「窗口边落在**屏边内侧 `snap_inset` 以内**，或者已经越过屏边不超过
/// `snap_dist`」。
///
/// ⚠ 注意这是个不对称的区间：拖拽期间窗口被 [`drag_position`] 限位在工作区内，
/// **不可能越过屏边**，所以 `snap_dist` 那 64px 在这条路径上根本够不着，
/// 实际生效的是内侧那 8px。之所以照样保留，是因为拖到边缘会被限位成完全贴边
/// （差值恰好 0），触发很稳定；改成 64px 会变成「离边缘 64px 就吸」，是另一种手感。
/// 要不要改是产品决定，不是迁移决定。
///
/// 四边按 左→右→上→下 顺序短路（与 Electron 版一致，角落同时够近时取左/右）。
pub fn detect_edge(b: Rect, wa: Rect, m: &Metrics) -> Option<Edge> {
    let in_range = |d: i32| d >= -m.snap_inset && d <= m.snap_dist;
    if in_range(wa.x - b.x) {
        Some(Edge::Left)
    } else if in_range(b.right() - wa.right()) {
        Some(Edge::Right)
    } else if in_range(wa.y - b.y) {
        Some(Edge::Top)
    } else if in_range(b.bottom() - wa.bottom()) {
        Some(Edge::Bottom)
    } else {
        None
    }
}

/// 取消吸附、恢复普通迷你小窗时重新限位（细条比小窗窄/短，必须重算）。
pub fn mini_resize_into(b: Rect, wa: Rect, m: &Metrics) -> Rect {
    let x = clamp(b.x, wa.x, wa.x + wa.w - m.mini_w);
    let y = clamp(b.y, wa.y, wa.y + wa.h - m.mini_h);
    Rect::new(x, y, m.mini_w, m.mini_h)
}

/// 拖拽跟随：光标位置减去按下时的偏移，再限位在基准屏工作区内。
pub fn drag_position(
    cursor: (i32, i32),
    offset: (i32, i32),
    size: (i32, i32),
    wa: Rect,
) -> (i32, i32) {
    let nx = clamp(cursor.0 - offset.0, wa.x, wa.x + wa.w - size.0);
    let ny = clamp(cursor.1 - offset.1, wa.y, wa.y + wa.h - size.1);
    (nx, ny)
}

/// 光标是否落在窗口外扩 `pad` 像素的范围内。
///
/// 两个用途：贴边细条只有几个像素，渲染层的 `pointerenter` 可能是误报，滑出前先校验；
/// 延时收回前也用同一套判断，避免「弹出又立刻收回」的抖动。
pub fn cursor_in_rect_padded(cursor: (f64, f64), b: Rect, pad: i32) -> bool {
    // 先排除 NaN：`NaN` 的任何比较都是 false，会一路走到「不在窗口上」，
    // 那就会误触收回 —— 拿不到光标位置时宁可当作「还在窗口上」。
    if !cursor.0.is_finite() || !cursor.1.is_finite() {
        return true;
    }
    let p = pad as f64;
    cursor.0 >= b.x as f64 - p
        && cursor.0 <= b.right() as f64 + p
        && cursor.1 >= b.y as f64 - p
        && cursor.1 <= b.bottom() as f64 + p
}

/// 光标是否已经深入基准屏之外（超过 `drag_switch_margin`）。
///
/// 拖拽期基准屏是「黏滞」的：只有光标真的跨过去才换屏，否则在多屏接缝处
/// 会逐帧翻转限位屏幕，窗口会突然跳到另一块屏上。
pub fn beyond_work_area(cursor: (i32, i32), wa: Rect, margin: i32) -> bool {
    cursor.0 < wa.x - margin
        || cursor.0 > wa.right() + margin
        || cursor.1 < wa.y - margin
        || cursor.1 > wa.bottom() + margin
}

/// 两个矩形的交集面积（无交集为 0）。
fn intersection_area(a: Rect, b: Rect) -> i64 {
    let w = (a.right().min(b.right()) - a.x.max(b.x)).max(0) as i64;
    let h = (a.bottom().min(b.bottom()) - a.y.max(b.y)).max(0) as i64;
    w * h
}

/// 挑出与 `rect` 交集最大的工作区 —— 等价 Electron 的 `screen.getDisplayMatching`。
///
/// 全都没交集时按「包含中心点」退化，再不行取第一个：**必须**返回一个工作区，
/// 否则调用方会退回主屏，多屏下窗口会跳到用户没在用的那块屏。
pub fn pick_work_area(rect: Rect, work_areas: &[Rect]) -> Option<Rect> {
    if work_areas.is_empty() {
        return None;
    }
    let mut best: Option<(i64, Rect)> = None;
    for wa in work_areas {
        let area = intersection_area(rect, *wa);
        if area > 0 && best.map_or(true, |(a, _)| area > a) {
            best = Some((area, *wa));
        }
    }
    if let Some((_, wa)) = best {
        return Some(wa);
    }
    let cx = rect.x + rect.w / 2;
    let cy = rect.y + rect.h / 2;
    work_areas
        .iter()
        .find(|wa| wa.contains(cx, cy))
        .copied()
        .or_else(|| work_areas.first().copied())
}

/// 包含给定点的那个工作区（等价 `screen.getDisplayMatching(1x1 矩形)`）。
///
/// 拖拽中判断「光标是不是跑到另一块屏上了」用。没有屏包含它时退回
/// [`pick_work_area`]，再不行返回 `None`，由调用方保留原基准屏（黏滞语义）。
pub fn work_area_at_point(x: i32, y: i32, work_areas: &[Rect]) -> Option<Rect> {
    work_areas
        .iter()
        .find(|wa| wa.contains(x, y))
        .copied()
        .or_else(|| pick_work_area(Rect::new(x, y, 1, 1), work_areas))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 100% 缩放：物理像素即 DIP，断言读起来就是设计值
    fn m100() -> Metrics {
        Metrics::for_scale(1.0)
    }

    /// 1920x1080 主屏，任务栏 40px 在底部
    fn wa_primary() -> Rect {
        Rect::new(0, 0, 1920, 1040)
    }
    /// 右侧副屏，坐标从 1920 开始
    fn wa_secondary() -> Rect {
        Rect::new(1920, 0, 1920, 1040)
    }

    // ---- Metrics ----

    #[test]
    fn metrics_at_100_percent_equals_design_values() {
        let m = m100();
        assert_eq!(m.mini_w, MINI_W_DIP);
        assert_eq!(m.mini_h, MINI_H_DIP);
        assert_eq!(m.full_w, FULL_W_DIP);
        assert_eq!(m.full_h, FULL_H_DIP);
        assert_eq!(m.dock_len, DOCK_LEN_DIP);
        assert_eq!(m.dock_pad_w, DOCK_PAD_W_DIP);
        assert_eq!(m.dock_pad_h, DOCK_PAD_H_DIP);
        assert_eq!(m.snap_dist, DOCK_SNAP_DIST_DIP);
        assert_eq!(m.snap_inset, DOCK_SNAP_INSET_DIP);
        assert_eq!(m.init_margin, INIT_MARGIN_DIP);
    }

    #[test]
    fn metrics_scale_up_at_125_percent() {
        // 125%：迷你窗在物理空间里变成 220x80 —— 这正是「不做换算窗口会小一圈」的原因
        let m = Metrics::for_scale(1.25);
        assert_eq!(m.mini_w, 220);
        assert_eq!(m.mini_h, 80);
        assert_eq!(m.full_w, 450);
        assert_eq!(m.dock_len, 95);
        assert_eq!(m.snap_inset, 10);
    }

    #[test]
    fn metrics_survive_bogus_scale_factor() {
        for bad in [0.0, -1.0, f64::NAN, f64::INFINITY] {
            let m = Metrics::for_scale(bad);
            assert_eq!(m, m100(), "缩放比 {bad} 应退回 1.0");
        }
        // 极小缩放比不能产生 0 尺寸的窗口
        let tiny = Metrics::for_scale(0.001);
        assert!(tiny.mini_w >= 1 && tiny.mini_h >= 1);
    }

    // ---- 初始摆放 / 迷你模式 ----

    #[test]
    fn initial_position_sits_in_bottom_right() {
        let (x, y) = initial_position(wa_primary(), FULL_W_DIP, FULL_H_DIP, &m100());
        assert_eq!(x, 1920 - FULL_W_DIP - 24);
        assert_eq!(y, 1040 - FULL_H_DIP - 24);
    }

    #[test]
    fn mini_anchors_to_top_right_of_full_window() {
        let full = Rect::new(1536, 456, FULL_W_DIP, FULL_H_DIP);
        let mini = mini_from_full(full, &m100());
        assert_eq!(mini, Rect::new(1720, 456, MINI_W_DIP, MINI_H_DIP));
        // 右上角对齐
        assert_eq!(mini.right(), full.right());
        assert_eq!(mini.y, full.y);
    }

    #[test]
    fn mini_to_full_keeps_top_right_anchor_when_room_allows() {
        let mini = Rect::new(1700, 456, MINI_W_DIP, MINI_H_DIP);
        let full = mini_to_full(mini, FULL_W_DIP, FULL_H_DIP, wa_primary());
        assert_eq!(full.right(), mini.right());
        assert_eq!(full.y, mini.y);
        assert_eq!(full.w, FULL_W_DIP);
        assert_eq!(full.h, FULL_H_DIP);
    }

    #[test]
    fn mini_to_full_clamps_into_work_area() {
        // 迷你窗贴着右下角 → 展开后必须被推回工作区内
        let mini = Rect::new(1744, 976, MINI_W_DIP, MINI_H_DIP);
        let full = mini_to_full(mini, FULL_W_DIP, FULL_H_DIP, wa_primary());
        assert!(full.right() <= wa_primary().right(), "展开后不该越过右边缘");
        assert!(full.bottom() <= wa_primary().bottom(), "展开后不该越过下边缘");
        assert_eq!(full.x, wa_primary().right() - FULL_W_DIP);
    }

    #[test]
    fn mini_to_full_survives_window_larger_than_work_area() {
        // 工作区比窗口还小（超小屏 / 极端缩放）：clamp 不 panic，退化成左上角
        let tiny = Rect::new(0, 0, 200, 200);
        let full = mini_to_full(Rect::new(50, 50, MINI_W_DIP, MINI_H_DIP), FULL_W_DIP, FULL_H_DIP, tiny);
        assert_eq!((full.x, full.y), (0, 0));
    }

    // ---- 贴边几何 ----

    #[test]
    fn dock_left_hidden_is_thin_tab_flush_to_edge() {
        let b = Rect::new(4, 300, MINI_W_DIP, MINI_H_DIP);
        let g = dock_geometry(Edge::Left, b, wa_primary(), true, &m100());
        assert_eq!(g, Rect::new(0, 300, DOCK_PAD_W_DIP, DOCK_LEN_DIP));
    }

    #[test]
    fn dock_left_shown_is_full_mini_flush_to_edge() {
        let b = Rect::new(0, 300, DOCK_PAD_W_DIP, DOCK_LEN_DIP);
        let g = dock_geometry(Edge::Left, b, wa_primary(), false, &m100());
        assert_eq!(g, Rect::new(0, 300, MINI_W_DIP, MINI_H_DIP));
    }

    #[test]
    fn dock_right_bottom_are_flush_to_far_edges() {
        let b = Rect::new(1500, 900, MINI_W_DIP, MINI_H_DIP);
        let hidden = dock_geometry(Edge::Right, b, wa_primary(), true, &m100());
        assert_eq!(hidden.x, 1920 - DOCK_PAD_W_DIP);
        let bottom = dock_geometry(Edge::Bottom, b, wa_primary(), true, &m100());
        assert_eq!(bottom.y, 1040 - DOCK_PAD_H_DIP);
        let bottom_shown = dock_geometry(Edge::Bottom, b, wa_primary(), false, &m100());
        assert_eq!(bottom_shown.y, 1040 - MINI_H_DIP);
    }

    #[test]
    fn dock_geometry_clamps_along_edge_direction() {
        // 越出工作区下方的迷你窗，收起时沿边方向要被夹回来（用 dock_len 夹）
        let b = Rect::new(500, 1030, MINI_W_DIP, MINI_H_DIP);
        let g = dock_geometry(Edge::Left, b, wa_primary(), true, &m100());
        assert_eq!(g.y, 1040 - DOCK_LEN_DIP);
        // 展开态用 mini_h 夹，位置不同 —— 这正是「收起/展开分别夹」的意义
        let g2 = dock_geometry(Edge::Left, b, wa_primary(), false, &m100());
        assert_eq!(g2.y, 1040 - MINI_H_DIP);
    }

    // ---- 吸附判定 ----

    #[test]
    fn detect_edge_snaps_when_flush_or_within_the_inset() {
        // 完全贴边（差值 0）→ 吸附。这是拖到边缘后最常走到的分支。
        assert_eq!(detect_edge(Rect::new(0, 300, MINI_W_DIP, MINI_H_DIP), wa_primary(), &m100()), Some(Edge::Left));
        // 内侧 8px 以内仍算吸附
        assert_eq!(detect_edge(Rect::new(8, 300, MINI_W_DIP, MINI_H_DIP), wa_primary(), &m100()), Some(Edge::Left));
        // 内侧 9px → 不吸附（这就是实际生效的阈值）
        assert_eq!(detect_edge(Rect::new(9, 300, MINI_W_DIP, MINI_H_DIP), wa_primary(), &m100()), None);
    }

    #[test]
    fn detect_edge_tolerates_at_most_64px_of_overshoot() {
        // 越过屏边不超过 64px 仍算吸附（多屏接缝处窗口可能被推到负坐标）。
        // 拖拽路径上到不了这里，但保留判定式与 Electron 版一致。
        assert_eq!(detect_edge(Rect::new(-64, 300, MINI_W_DIP, MINI_H_DIP), wa_primary(), &m100()), Some(Edge::Left));
        assert_eq!(detect_edge(Rect::new(-65, 300, MINI_W_DIP, MINI_H_DIP), wa_primary(), &m100()), None);
    }

    #[test]
    fn detect_edge_right_side_uses_the_same_band() {
        // 贴右边缘：b.right == wa.right() → 差值 0
        assert_eq!(
            detect_edge(Rect::new(1920 - MINI_W_DIP, 300, MINI_W_DIP, MINI_H_DIP), wa_primary(), &m100()),
            Some(Edge::Right)
        );
        // 距右边缘 20px：落在 8px 带之外 → 不吸附
        assert_eq!(
            detect_edge(
                Rect::new(1920 - MINI_W_DIP - 20, 300, MINI_W_DIP, MINI_H_DIP),
                wa_primary(),
                &m100()
            ),
            None
        );
    }

    #[test]
    fn detect_edge_left_beats_right_when_window_fills_work_area() {
        // 窗口恰好铺满工作区：四边都在范围内，按顺序应命中 left
        let b = Rect::new(0, 0, 1920, 1040);
        assert_eq!(detect_edge(b, wa_primary(), &m100()), Some(Edge::Left));
    }

    #[test]
    fn detect_edge_none_in_the_middle_of_screen() {
        assert_eq!(
            detect_edge(Rect::new(800, 500, MINI_W_DIP, MINI_H_DIP), wa_primary(), &m100()),
            None
        );
    }

    #[test]
    fn mini_resize_into_pulls_tab_back_into_work_area() {
        // 从左侧细条拖离边缘：细条宽 34 → 恢复 176，必须重新夹回屏内
        let tab = Rect::new(0, 300, DOCK_PAD_W_DIP, DOCK_LEN_DIP);
        let g = mini_resize_into(tab, wa_primary(), &m100());
        assert_eq!(g, Rect::new(0, 300, MINI_W_DIP, MINI_H_DIP));
        assert!(g.right() <= wa_primary().right());
    }

    // ---- 拖拽 ----

    #[test]
    fn drag_position_clamps_to_base_screen() {
        let wa = wa_primary();
        // 光标跑到屏外 → 窗口被夹在右下角
        let (x, y) = drag_position((5000, 5000), (10, 10), (MINI_W_DIP, MINI_H_DIP), wa);
        assert_eq!((x, y), (1920 - MINI_W_DIP, 1040 - MINI_H_DIP));
        // 光标在左上角外 → 夹在 (0,0)
        let (x, y) = drag_position((-100, -100), (10, 10), (MINI_W_DIP, MINI_H_DIP), wa);
        assert_eq!((x, y), (0, 0));
    }

    #[test]
    fn drag_position_keeps_grab_offset() {
        // 在窗口内 (30, 20) 处按下，光标移动到 (400, 300) → 窗口左上角应为 (370, 280)
        let (x, y) = drag_position((400, 300), (30, 20), (MINI_W_DIP, MINI_H_DIP), wa_primary());
        assert_eq!((x, y), (370, 280));
    }

    #[test]
    fn beyond_work_area_needs_real_travel() {
        let wa = wa_primary();
        let m = m100();
        // 贴着右边缘外一点点：不算越界
        assert!(!beyond_work_area((1925, 500), wa, m.drag_switch_margin));
        // 边界是**严格**大于（`cursor > right + margin`）：正好超出 40px 还不算，
        // 超出 41px 才算。写死这条免得以后有人手滑把 `>` 改成 `>=`。
        assert!(!beyond_work_area((1960, 500), wa, m.drag_switch_margin));
        assert!(beyond_work_area((1961, 500), wa, m.drag_switch_margin));
        assert!(!beyond_work_area((0, -40), wa, m.drag_switch_margin));
        assert!(beyond_work_area((0, -41), wa, m.drag_switch_margin));
    }

    #[test]
    fn cursor_near_window_tolerates_dpi_rounding() {
        let b = Rect::new(100, 200, MINI_W_DIP, MINI_H_DIP);
        assert!(cursor_in_rect_padded((100.0, 200.0), b, DOCK_REVEAL_PAD));
        // 比窗口左边缘外 8px 整：仍算在内（边界包含）
        assert!(cursor_in_rect_padded((92.0, 200.0), b, DOCK_REVEAL_PAD));
        assert!(!cursor_in_rect_padded((91.0, 200.0), b, DOCK_REVEAL_PAD));
        // 右下角同理
        assert!(cursor_in_rect_padded((284.0, 272.0), b, DOCK_REVEAL_PAD));
        assert!(!cursor_in_rect_padded((285.0, 272.0), b, DOCK_REVEAL_PAD));
    }

    #[test]
    fn cursor_near_window_treats_unknown_position_as_inside() {
        // 拿不到光标位置时按「还在窗口上」处理，避免误触「延时收回」
        let b = Rect::new(0, 0, 100, 100);
        assert!(cursor_in_rect_padded((f64::NAN, f64::NAN), b, DOCK_HIDE_PAD));
        assert!(cursor_in_rect_padded((f64::INFINITY, 0.0), b, DOCK_HIDE_PAD));
    }

    // ---- 屏幕挑选 ----

    #[test]
    fn pick_work_area_uses_largest_intersection() {
        let areas = [wa_primary(), wa_secondary()];
        // 窗口大部分在副屏上
        assert_eq!(pick_work_area(Rect::new(1900, 100, 300, 300), &areas), Some(wa_secondary()));
        // 窗口大部分在主屏上（跨接缝）
        assert_eq!(pick_work_area(Rect::new(1700, 100, 300, 300), &areas), Some(wa_primary()));
    }

    #[test]
    fn pick_work_area_falls_back_when_fully_offscreen() {
        let areas = [wa_primary()];
        // 完全在屏幕外（拔掉副屏后残留坐标）：仍要返回一个工作区，不能 None
        assert_eq!(pick_work_area(Rect::new(5000, 5000, 100, 100), &areas), Some(wa_primary()));
    }

    #[test]
    fn pick_work_area_handles_empty_monitor_list() {
        assert_eq!(pick_work_area(Rect::new(0, 0, 10, 10), &[]), None);
    }

    #[test]
    fn work_area_at_point_prefers_the_containing_screen() {
        let areas = [wa_primary(), wa_secondary()];
        assert_eq!(work_area_at_point(100, 100, &areas), Some(wa_primary()));
        assert_eq!(work_area_at_point(2000, 100, &areas), Some(wa_secondary()));
        // 屏幕间的空隙：退化成最近一块，而不是 None（否则调用方会保留旧基准屏）
        assert_eq!(work_area_at_point(1919, 5000, &areas), Some(wa_primary()));
    }

    #[test]
    fn negative_coordinates_work_for_left_of_primary_monitor() {
        // 副屏放在主屏左边时坐标为负 —— 夹取逻辑不能假设坐标非负
        let left_screen = Rect::new(-1920, 0, 1920, 1040);
        let mini = Rect::new(-1920, 300, MINI_W_DIP, MINI_H_DIP);
        let g = dock_geometry(Edge::Left, mini, left_screen, true, &m100());
        assert_eq!(g.x, -1920);
        let full = mini_to_full(mini, FULL_W_DIP, FULL_H_DIP, left_screen);
        assert!(full.x >= -1920);
    }
}
