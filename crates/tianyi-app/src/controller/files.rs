//! 目录浏览与文件操作：空间 / 面包屑 / 行模型 / 选择 / 排序 / 搜索 / 打开 / 重命名 / 复制 / 移动 / 删除 / 直链 / 预览 / 容量。
use super::*;
use crate::app::{CrumbEntry, DirectLink, FileEntry};
use slint::SharedString;
use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::atomic::Ordering;
use std::sync::Arc;
use tianyi_core::client::TianyiClient;
use tianyi_core::file::{self, FileKind};
use tianyi_core::models::FileObject;
use tianyi_core::task::{Task, TaskDirection, TaskStatus};
impl Controller {
    // ==================================================================
    // 目录浏览
    // ==================================================================
    fn space_label(&self) -> &'static str {
        if lock_or_recover(&self.space).as_str() == "family" {
            "家庭云"
        } else {
            "个人云"
        }
    }

    /// 构建面包屑：根 + 已进入的目录栈
    fn crumbs(&self) -> Vec<CrumbEntry> {
        let mut out = vec![CrumbEntry {
            id: ROOT_FOLDER_ID.into(),
            name: self.space_label().into(),
        }];
        for (id, name) in lock_or_recover(&self.path_stack).iter() {
            out.push(CrumbEntry {
                id: id.clone().into(),
                name: name.clone().into(),
            });
        }
        out
    }

    fn push_crumbs(&self) {
        let crumbs = self.crumbs();
        self.set_ui_state(move |ui| {
            ui.set_crumbs(slint::ModelRc::new(slint::VecModel::from(crumbs)));
        });
    }

    /// 当前应当显示的行：处于搜索态时显示搜索结果，否则显示目录内容。
    ///
    /// 抽成纯函数，保证“清空搜索后恢复目录内容”这一行为有唯一实现。
    pub(super) fn rows_for_display(
        dir: &[FileObject],
        search: Option<&[FileObject]>,
    ) -> Vec<FileObject> {
        match search {
            Some(hits) => hits.to_vec(),
            None => dir.to_vec(),
        }
    }

    /// 把行数据推送到 UI，并按当前选择集标记 selected
    fn push_rows(&self, rows: &[FileObject]) {
        let selected: HashSet<String> = lock_or_recover(&self.selected_ids)
            .iter()
            .cloned()
            .collect();
        let entries = files_to_entries(rows, &selected);
        *lock_or_recover(&self.entry_cache) = entries.clone();
        let count = selected.len() as i32;
        self.set_ui_state(move |ui| {
            apply_entries(ui, entries, count);
        });
    }

    /// 清空列表模型（换云空间时用）
    fn clear_entries(&self) {
        *lock_or_recover(&self.entry_cache) = Vec::new();
        self.set_ui_state(|ui| {
            ui.set_file_list(slint::ModelRc::default());
        });
    }

    /// 更新选择集
    ///
    /// 这是选择状态的唯一事实来源：UI 侧只读 FileEntry.selected 与 selected-count，
    /// 不允许自己维护一份"看起来选中了"的状态，避免出现界面多选、操作只作用于一个文件。
    pub(super) fn set_selection(&self, ids: Vec<String>) {
        *lock_or_recover(&self.selected_ids) = ids.clone();
        let set: HashSet<String> = ids.iter().cloned().collect();
        let mut cache = lock_or_recover(&self.entry_cache);
        for e in cache.iter_mut() {
            e.selected = set.contains(e.id.as_str());
        }
        let entries = cache.clone();
        drop(cache);
        let count = ids.len() as i32;
        // 只改 selected 标记：走就地更新，行实例（和它的双击判定）不受影响
        self.set_ui_state(move |ui| {
            apply_entries(ui, entries, count);
        });
    }

    /// 丢弃已不在列表里的选中项（不会触发额外的 UI 更新）
    fn retain_selection(&self, rows: &[FileObject]) {
        let live: HashSet<&str> = rows.iter().map(|f| f.id.as_str()).collect();
        lock_or_recover(&self.selected_ids).retain(|id| live.contains(id.as_str()));
    }

    fn sort_key_pair(&self) -> (i32, bool) {
        (
            *lock_or_recover(&self.sort_key),
            *lock_or_recover(&self.sort_asc),
        )
    }

    fn sorted_rows(&self, rows: &mut [FileObject]) {
        let (key, asc) = self.sort_key_pair();
        sort_files(rows, key, asc);
    }

    fn push_sort_state(&self) {
        let (key, asc) = self.sort_key_pair();
        self.set_ui_state(move |ui| {
            ui.set_sort_key(key);
            ui.set_sort_asc(asc);
        });
    }

    /// 列表状态与状态栏文案一起更新，避免"加载中"标志和文案不同步
    pub(super) fn set_list_state(&self, state: &str, status: &str) {
        let state = state.to_string();
        let status = status.to_string();
        self.set_ui_state(move |ui| {
            ui.set_list_state(SharedString::from(state));
            ui.set_status_text(SharedString::from(status));
        });
    }

    pub(super) fn set_file_error(&self, msg: impl Into<String>) {
        let msg = msg.into();
        self.set_ui_state(move |ui| {
            ui.set_file_error(SharedString::from(msg));
        });
    }

    /// 切换排序：同一列再次点击变为反向
    pub(super) fn sort_changed(self: &Arc<Self>, key: i32) {
        let key = key.clamp(0, 2);
        {
            let mut cur = lock_or_recover(&self.sort_key);
            let mut asc = lock_or_recover(&self.sort_asc);
            if *cur == key {
                *asc = !*asc;
            } else {
                *cur = key;
                *asc = true;
            }
        }
        let mut rows = lock_or_recover(&self.visible_rows).clone();
        self.sorted_rows(&mut rows);
        *lock_or_recover(&self.visible_rows) = rows.clone();
        {
            let mut dir = lock_or_recover(&self.dir_rows);
            self.sorted_rows(&mut dir);
        }
        let (key, asc) = self.sort_key_pair();
        self.push_sort_state();
        self.push_rows(&rows);
        self.set_status(format!(
            "已按{} {}排序",
            sort_label(key),
            if asc { "升序" } else { "降序" }
        ));
    }

    /// 加载当前目录（F5 / 刷新按钮 / 操作后刷新）
    ///
    /// 同一目录的刷新保留选中项与已有列表内容，只给非侵入式刷新指示；
    /// 目录切换由 `load_dir(false)` 走另一条路径。
    pub fn refresh_files(self: &Arc<Self>) {
        self.load_dir(true)
    }

    /// 重新加载当前目录
    ///
    /// keep_selection=true → 视为刷新：保留选中项与列表内容
    /// keep_selection=false → 视为目录切换：清空选择，重新进入加载态
    pub(super) fn load_dir(self: &Arc<Self>, keep_selection: bool) {
        let gen = self.dir_gen.fetch_add(1, Ordering::SeqCst) + 1;
        // 任何目录加载都会终止搜索结果
        self.search_gen.fetch_add(1, Ordering::SeqCst);
        if !keep_selection {
            self.set_selection(Vec::new());
        }
        let Some(client) = self.client() else {
            self.set_list_state("error", "未登录");
            self.set_file_error("未登录");
            return;
        };
        let had_rows = !lock_or_recover(&self.visible_rows).is_empty();
        let silent = keep_selection && had_rows;
        let folder_id = lock_or_recover(&self.folder_id).clone();
        let this = Arc::clone(self);
        let backend = self.backend.clone();

        self.push_crumbs();
        self.push_sort_state();
        self.set_ui_state(move |ui| {
            ui.set_searching(false);
            ui.set_renaming_id(SharedString::default());
            ui.set_file_error(SharedString::default());
        });
        self.set_list_state(
            if silent { "refreshing" } else { "loading" },
            if silent {
                "正在刷新目录…"
            } else {
                "正在加载目录…"
            },
        );

        backend.spawn(move || async move {
            match client.list_files(&folder_id).await {
                Ok(mut files) => {
                    if this.dir_gen.load(Ordering::SeqCst) != gen {
                        return; // 已被更新的请求取代
                    }
                    this.sorted_rows(&mut files);
                    let count = files.len();
                    *lock_or_recover(&this.dir_rows) = files.clone();
                    *lock_or_recover(&this.visible_rows) = files.clone();
                    // 目录内容变了：丢掉已不存在的选中项，保证后续操作对象始终有效
                    this.retain_selection(&files);
                    this.push_rows(&files);
                    this.set_list_state(
                        if count == 0 { "empty" } else { "ready" },
                        &format!("{count} 个项目"),
                    );
                }
                Err(e) => {
                    if this.dir_gen.load(Ordering::SeqCst) != gen {
                        return;
                    }
                    let msg = format!("加载目录失败：{e}");
                    this.set_file_error(msg.clone());
                    this.set_list_state("error", &msg);
                }
            }
        });
    }

    /// 取消在途加载（加载态上的"取消"入口）
    ///
    /// 不新增"取消网络请求"的机制：两颗代数各加一次，在途结果回来时
    /// `load_dir` / `search_changed` 里的 `*_gen.load() != gen` 判定就把它丢掉，
    /// 走的是既有的丢结果路径。列表状态就地归一，用户不必再对着转圈等下去。
    ///
    /// 取消按钮只在 `show-loading`（list_state == "loading" 且当前没有任何行）时
    /// 可见，所以实际几乎总落到 `empty`；`ready` 那一支留给"取消的同一帧里结果
    /// 已经先到"的情形，免得把刚到手的行又藏起来。
    pub(super) fn cancel_loading(self: &Arc<Self>) {
        self.dir_gen.fetch_add(1, Ordering::SeqCst);
        self.search_gen.fetch_add(1, Ordering::SeqCst);
        let count = lock_or_recover(&self.visible_rows).len();
        self.set_list_state(cancelled_list_state(count), &format!("{count} 个项目"));
        self.notice("info", "已取消加载");
    }

    pub(super) fn load_capacity(self: &Arc<Self>) {
        let gen = self.space_gen.load(Ordering::SeqCst);
        let Some(client) = self.client() else { return };
        let this = Arc::clone(self);
        self.backend.spawn(move || async move {
            let Ok(cap) = client.get_capacity().await else {
                return;
            };
            if this.space_gen.load(Ordering::SeqCst) != gen {
                return;
            }
            let (total, used) = if client.is_family() {
                (
                    cap.family_capacity_info.total_size,
                    cap.family_capacity_info.used_size,
                )
            } else {
                (
                    cap.cloud_capacity_info.total_size,
                    cap.cloud_capacity_info.used_size,
                )
            };
            let total_s = file::format_size(total.max(0) as u64);
            let used_s = file::format_size(used.max(0) as u64);
            let ratio = if total > 0 {
                (used.max(0) as f32 / total as f32).clamp(0.0, 1.0)
            } else {
                0.0
            };
            this.set_ui_state(move |ui| {
                ui.set_total_space(SharedString::from(total_s));
                ui.set_used_space(SharedString::from(used_s));
                ui.set_capacity_ratio(ratio);
            });
        });
    }

    pub(super) fn navigate_crumb(self: &Arc<Self>, index: usize) {
        // index 0 = 根目录
        let mut stack = lock_or_recover(&self.path_stack);
        if index == 0 {
            stack.clear();
            *lock_or_recover(&self.folder_id) = ROOT_FOLDER_ID.to_string();
        } else if index <= stack.len() {
            stack.truncate(index);
            let (id, _) = stack.last().cloned().unwrap();
            *lock_or_recover(&self.folder_id) = id;
        } else {
            return;
        }
        drop(stack);
        self.load_dir(false);
    }

    pub(super) fn navigate_up(self: &Arc<Self>) {
        let mut stack = lock_or_recover(&self.path_stack);
        if stack.pop().is_some() {
            let next = stack
                .last()
                .map(|(id, _)| id.clone())
                .unwrap_or_else(|| ROOT_FOLDER_ID.to_string());
            *lock_or_recover(&self.folder_id) = next;
            drop(stack);
            self.load_dir(false);
        }
    }

    /// 「再点一次同一个云空间」是否真的需要执行切换。
    ///
    /// `true` = 必须走完整切换（清路径栈 / 行缓存 / 选择集，并发起目录请求）；
    /// `false` = 这次点击是空操作，调用方原样返回即可。
    ///
    /// 抽成纯函数是为了让这条判定能被单测直接覆盖：真跑一遍 `switch_space` 需要一个
    /// UI 窗口加一个后端客户端，而要守住的恰恰是「不发请求、不清状态」这个**负向**
    /// 行为——用副作用去察觉"什么都没发生"本来就不可靠。
    ///
    /// `current_space` 与 `target` 都应是归一化后的空间名（personal / family）；
    /// `at_root` = 当前 folder_id 是根目录且路径栈为空；`searching` = 当前处于搜索态。
    pub(super) fn should_switch_space(
        current_space: &str,
        at_root: bool,
        searching: bool,
        target: &str,
    ) -> bool {
        current_space != target || !at_root || searching
    }

    pub(super) fn switch_space(self: &Arc<Self>, space: &str) {
        let space = if space == "family" {
            "family"
        } else {
            "personal"
        }
        .to_string();

        // —— 空操作早退 ——
        // 导航栏的「个人云 / 家庭云」每点一次都走这里。原实现无条件清空
        // path_stack / dir_rows / visible_rows / 选择集与搜索代际，于是在
        // 「个人云」根目录再点一次「个人云」= 选择被清空 + 一次多余的网络往返，
        // 而用户根本没有改变任何东西。页面切换由 UI 的 current-page 负责，不依赖这里。
        //
        // 只挡「同一空间 + 已在根目录 + 非搜索态」这一种：子目录里点回根目录
        // 仍然要切换（那是真实的目录变更），必须保留。
        let current_space = lock_or_recover(&self.space).clone();
        let at_root = lock_or_recover(&self.folder_id).as_str() == ROOT_FOLDER_ID
            && lock_or_recover(&self.path_stack).is_empty();
        // 搜索态是 UI 侧属性：只有站在 UI 线程上才读得到。读不到（跨线程调用这条
        // 路径本来也存在）时按"正在搜索"处理——宁可多刷一次，也不要把搜索结果
        // 静默留在屏幕上，那看起来像"点了没反应"。
        let searching = match self.upgrade_on_ui_thread() {
            Some(ui) => ui.get_searching(),
            None => true,
        };
        if !Self::should_switch_space(&current_space, at_root, searching, &space) {
            return;
        }

        *lock_or_recover(&self.space) = space.clone();
        let gen = self.space_gen.fetch_add(1, Ordering::SeqCst) + 1;
        self.dir_gen.fetch_add(1, Ordering::SeqCst);
        self.search_gen.fetch_add(1, Ordering::SeqCst);
        *lock_or_recover(&self.folder_id) = ROOT_FOLDER_ID.to_string();
        lock_or_recover(&self.path_stack).clear();
        lock_or_recover(&self.dir_rows).clear();
        lock_or_recover(&self.visible_rows).clear();

        let Some(client) = self.client() else { return };
        client.set_account(|acc| acc.space_type = space.clone());

        // 先清空模型与行缓存，再清选择集：反过来的话 set_selection 会拿旧行的
        // 缓存再做一次就地更新，纯属白干
        self.clear_entries();
        self.set_selection(Vec::new());
        self.set_ui_state(|ui| {
            ui.set_searching(false);
            ui.set_search_keyword(SharedString::default());
            ui.set_renaming_id(SharedString::default());
            ui.set_file_error(SharedString::default());
        });
        self.set_list_state("loading", "正在切换云空间…");
        self.push_crumbs();

        if space == "personal" {
            self.load_dir(false);
            self.load_capacity();
            return;
        }

        // 家庭云需要先拿到 familyId
        let this = Arc::clone(self);
        self.backend.spawn(move || async move {
            match client.ensure_family_id().await {
                Ok(_) => {
                    if this.space_gen.load(Ordering::SeqCst) != gen {
                        return;
                    }
                    this.load_dir(false);
                    this.load_capacity();
                }
                Err(e) => {
                    if this.space_gen.load(Ordering::SeqCst) != gen {
                        return;
                    }
                    // 回退个人云，并如实告知失败原因
                    if let Some(c) = this.client() {
                        c.set_account(|acc| acc.space_type = "personal".to_string());
                    }
                    *lock_or_recover(&this.space) = "personal".to_string();
                    this.set_ui_state(|ui| {
                        ui.set_current_page(0);
                    });
                    this.notice("error", format!("家庭云不可用，已返回个人云：{e}"));
                    this.load_dir(false);
                    this.load_capacity();
                }
            }
        });
    }

    pub(super) fn handle_open_item(self: &Arc<Self>, id: &str, name: &str, is_dir: bool) {
        if !is_dir {
            self.preview_file(id, name);
            return;
        }
        let current = lock_or_recover(&self.folder_id).clone();
        lock_or_recover(&self.path_stack).push((current, name.to_string()));
        *lock_or_recover(&self.folder_id) = id.to_string();
        self.load_dir(false);
    }

    /// Enter 键的「打开」：与双击同义，作用于当前唯一选中项。
    ///
    /// 目标从选择集（`targets()`）取，而不是让 UI 自己去列表里猜哪一行被选中——
    /// 选择状态只有一份事实来源。
    pub(super) fn open_selected(self: &Arc<Self>) {
        let targets = self.targets();
        let [item] = targets.as_slice() else {
            return;
        };
        self.handle_open_item(&item.id, &item.name, item.is_dir);
    }

    /// 单击选择：普通点击=单选，Ctrl=切换，Shift=从锚点范围选择
    pub(super) fn handle_select_item(self: &Arc<Self>, id: &str, ctrl: bool, shift: bool) {
        let rows = lock_or_recover(&self.visible_rows).clone();
        if rows.is_empty() {
            return;
        }
        let anchor = lock_or_recover(&self.selection_anchor).clone();
        let mut ids = lock_or_recover(&self.selected_ids).clone();

        if shift && !anchor.is_empty() && !ctrl {
            // 范围选择：用锚点到目标之间的连续区间替换当前选择
            if let Some(range) = range_selection(&rows, &anchor, id) {
                self.set_selection(range);
                self.set_ui_state(|ui| ui.set_renaming_id(SharedString::default()));
                return;
            }
            // 锚点已被刷新掉：退化为单选
        }

        if ctrl {
            if let Some(pos) = ids.iter().position(|x| x == id) {
                ids.remove(pos);
            } else {
                ids.push(id.to_string());
            }
            *lock_or_recover(&self.selection_anchor) = id.to_string();
            self.set_selection(ids);
            self.set_ui_state(|ui| ui.set_renaming_id(SharedString::default()));
            return;
        }

        *lock_or_recover(&self.selection_anchor) = id.to_string();
        self.set_selection(vec![id.to_string()]);
        self.set_ui_state(|ui| ui.set_renaming_id(SharedString::default()));
    }

    /// 全选当前目录 / 搜索结果
    pub(super) fn select_all_items(self: &Arc<Self>) {
        let ids: Vec<String> = lock_or_recover(&self.visible_rows)
            .iter()
            .map(|f| f.id.clone())
            .collect();
        if ids.is_empty() {
            return;
        }
        let count = ids.len();
        self.set_selection(ids);
        self.set_status(format!("已全选 {count} 项"));
    }

    pub(super) fn handle_context(
        self: &Arc<Self>,
        id: &str,
        name: &str,
        is_dir: bool,
        x: f32,
        y: f32,
    ) {
        // 右键的行若不在当前选择里，则先把选择切到这一行，
        // 保证菜单里的操作对象与用户看到的选中项一致。
        let in_selection = lock_or_recover(&self.selected_ids).iter().any(|s| s == id);
        if !in_selection {
            *lock_or_recover(&self.selection_anchor) = id.to_string();
            self.set_selection(vec![id.to_string()]);
        }
        let id_s = id.to_string();
        let name_s = name.to_string();
        self.set_ui_state(move |ui| {
            ui.set_renaming_id(SharedString::default());
            ui.set_context_open(true);
            ui.set_context_x(x);
            ui.set_context_y(y);
            ui.set_context_item_id(SharedString::from(id_s));
            ui.set_context_item_name(SharedString::from(name_s));
            ui.set_context_is_dir(is_dir);
        });
    }

    /// 当前操作的目标文件集合（全部来自选择模型的唯一事实来源）
    fn targets(&self) -> Vec<FileObject> {
        let ids = lock_or_recover(&self.selected_ids).clone();
        if ids.is_empty() {
            return Vec::new();
        }
        let set: HashSet<String> = ids.into_iter().collect();
        lock_or_recover(&self.visible_rows)
            .iter()
            .filter(|f| set.contains(&f.id))
            .cloned()
            .collect()
    }

    // ==================================================================
    // 文件操作
    // ==================================================================
    pub(super) fn download_selected(self: &Arc<Self>) {
        let targets: Vec<FileObject> = self.targets().into_iter().filter(|f| !f.is_dir).collect();
        if targets.is_empty() {
            self.notice("info", "请先选择要下载的文件（文件夹请进入后再选择文件）");
            return;
        }
        let default_dir = self
            .store
            .load_config()
            .map(|c| c.download_dir)
            .unwrap_or_else(|_| PathBuf::from("."));
        if targets.len() == 1 {
            let f = &targets[0];
            let Some(dest) = rfd::FileDialog::new()
                .set_title("选择下载位置")
                .set_directory(&default_dir)
                .set_file_name(&f.name)
                .save_file()
            else {
                return;
            };
            self.enqueue_download(f.clone(), dest);
            return;
        }
        // 多选：选一个目录，按文件名保存
        let Some(dir) = rfd::FileDialog::new()
            .set_title(format!("选择下载目录（{} 个文件）", targets.len()))
            .set_directory(&default_dir)
            .pick_folder()
        else {
            return;
        };
        let count = targets.len();
        for f in targets {
            let dest = dir.join(sanitize_file_name(&f.name));
            self.enqueue_download(f, dest);
        }
        self.notice("info", format!("已加入 {count} 个下载任务"));
    }

    fn enqueue_download(self: &Arc<Self>, file: FileObject, dest: PathBuf) {
        let task_id = self.tasks.next_id();
        let size = file.size.max(0) as u64;
        let task = Task {
            id: task_id,
            direction: TaskDirection::Download,
            file_name: file.name.clone(),
            file_size: size,
            local_path: dest,
            remote_folder_id: file.id.clone(),
            status: TaskStatus::Running,
            progress: 0.0,
            bytes_done: 0,
            speed: 0,
            last_done: 0,
            last_speed_time: 0,
            error: None,
            created_at: now_secs(),
        };
        self.tasks.add(task);
        self.set_ui_state(|ui| ui.set_transfer_panel_open(true));
        self.spawn_transfer(task_id);
        self.refresh_transfers();
    }

    pub(super) fn rename_selected(self: &Arc<Self>) {
        let targets = self.targets();
        if targets.len() != 1 {
            self.notice("info", "重命名只支持单个文件");
            return;
        }
        let id = targets[0].id.clone();
        self.set_ui_state(move |ui| {
            ui.set_renaming_id(SharedString::from(id));
            ui.set_status_text(SharedString::from("输入新名称后按回车保存"));
        });
    }

    pub(super) fn rename_submitted(self: &Arc<Self>, id: &str, new_name: &str) {
        let new_name = new_name.trim().to_string();
        let Some(file) = self.find_visible(id) else {
            self.set_ui_state(|ui| ui.set_renaming_id(SharedString::default()));
            return;
        };
        if new_name.is_empty() {
            self.set_ui_state(|ui| {
                ui.set_renaming_id(SharedString::default());
            });
            self.notice("error", "名称不能为空");
            return;
        }
        if new_name == file.name {
            self.set_ui_state(|ui| ui.set_renaming_id(SharedString::default()));
            return;
        }
        let Some(client) = self.client() else { return };
        let this = Arc::clone(self);
        self.backend.spawn(move || async move {
            match client.rename(&file, &new_name).await {
                Ok(_) => {
                    this.set_ui_state(|ui| ui.set_renaming_id(SharedString::default()));
                    this.notice("success", format!("已重命名为 {new_name}"));
                    this.refresh_files();
                }
                Err(e) => {
                    this.set_ui_state(|ui| ui.set_renaming_id(SharedString::default()));
                    this.notice("error", format!("重命名失败：{e}"));
                }
            }
        });
    }

    /// Esc 取消重命名：不碰后端，只把行从输入态退回展示态
    ///
    /// 必须与 rename_submitted 走同一条"清空 renaming-id"的路径——
    /// 行内 TouchArea 在 renaming 时被禁用，不清空就再也点不回来。
    pub(super) fn rename_cancelled(&self, id: &str) {
        log::debug!("rename cancelled: {id}");
        let (renaming_id, status) = rename_cancelled_state();
        self.set_ui_state(move |ui| {
            ui.set_renaming_id(renaming_id);
            ui.set_status_text(SharedString::from(status));
        });
    }

    pub(super) fn copy_selected(self: &Arc<Self>) {
        let targets = self.targets();
        if targets.is_empty() {
            return;
        }
        let dst = lock_or_recover(&self.folder_id).clone();
        let Some(client) = self.client() else { return };
        let count = targets.len();
        let this = Arc::clone(self);
        self.backend.spawn(move || async move {
            let mut failed = Vec::new();
            for f in targets {
                if let Err(e) = client.copy_to(&f, &dst).await {
                    failed.push(format!("{}: {e}", f.name));
                }
            }
            if failed.is_empty() {
                this.notice("success", format!("已复制 {count} 项到当前目录"));
                this.refresh_files();
            } else {
                this.notice("error", format!("部分复制失败：{}", failed.join("；")));
                this.refresh_files();
            }
        });
    }

    pub(super) fn move_selected_up(self: &Arc<Self>) {
        let parent_id = {
            let stack = lock_or_recover(&self.path_stack);
            if stack.len() < 2 {
                // 栈中只有一项时，上级就是根目录
                if stack.is_empty() {
                    return;
                }
                ROOT_FOLDER_ID.to_string()
            } else {
                stack[stack.len() - 2].0.clone()
            }
        };
        let targets = self.targets();
        if targets.is_empty() {
            return;
        }
        let Some(client) = self.client() else { return };
        let count = targets.len();
        let this = Arc::clone(self);
        self.backend.spawn(move || async move {
            let mut failed = Vec::new();
            for f in targets {
                if let Err(e) = client.move_to(&f, &parent_id).await {
                    failed.push(format!("{}: {e}", f.name));
                }
            }
            if failed.is_empty() {
                this.notice("success", format!("已移动 {count} 项到上级目录"));
            } else {
                this.notice("error", format!("部分移动失败：{}", failed.join("；")));
            }
            this.refresh_files();
        });
    }

    /// 删除：先弹出应用内确认对话框（同步的 rfd::MessageDialog 会阻塞 UI 线程），
    /// 用户确认后才真正执行，取消则完全不动数据。
    pub(super) fn delete_selected(self: &Arc<Self>) {
        let targets = self.targets();
        if targets.is_empty() {
            self.notice("info", "请先选择要删除的项目");
            return;
        }
        let message = if targets.len() == 1 {
            let t = &targets[0];
            if t.is_dir {
                format!(
                    "确定要删除文件夹「{}」吗？其中的内容会一并删除，且无法恢复。",
                    t.name
                )
            } else {
                format!("确定要删除「{}」吗？此操作无法恢复。", t.name)
            }
        } else {
            format!(
                "确定要删除选中的 {} 个项目吗？此操作无法恢复。",
                targets.len()
            )
        };
        let title = if targets.len() == 1 {
            "确认删除".to_string()
        } else {
            format!("确认删除 {} 个项目", targets.len())
        };
        *lock_or_recover(&self.pending_confirm) = Some(PendingConfirm::DeleteFiles(targets));
        self.set_ui_state(move |ui| {
            ui.set_dialog_title(SharedString::from(title));
            ui.set_dialog_message(SharedString::from(message));
            ui.set_dialog_confirm_text(SharedString::from("删除"));
            ui.set_dialog_danger(true);
            ui.set_dialog_open(true);
        });
    }

    pub(super) fn dialog_cancel(self: &Arc<Self>) {
        *lock_or_recover(&self.pending_confirm) = None;
        self.set_ui_state(|ui| ui.set_dialog_open(false));
    }

    pub(super) fn dialog_confirm(self: &Arc<Self>) {
        let action = lock_or_recover(&self.pending_confirm).take();
        self.set_ui_state(|ui| ui.set_dialog_open(false));
        match action {
            Some(PendingConfirm::DeleteFiles(targets)) => self.perform_delete(targets),
            None => {}
        }
    }

    fn perform_delete(self: &Arc<Self>, targets: Vec<FileObject>) {
        let Some(client) = self.client() else { return };
        let count = targets.len();
        let this = Arc::clone(self);
        self.set_status(format!("正在删除 {count} 项…"));
        self.backend.spawn(move || async move {
            let mut failed = Vec::new();
            for f in targets {
                if let Err(e) = client.delete(&f).await {
                    failed.push(format!("{}: {e}", f.name));
                }
            }
            if failed.is_empty() {
                this.notice("success", format!("已删除 {count} 项"));
            } else {
                this.notice("error", format!("部分删除失败：{}", failed.join("；")));
            }
            this.refresh_files();
        });
    }

    pub(super) fn link_selected(self: &Arc<Self>) {
        let targets: Vec<FileObject> = self.targets().into_iter().filter(|f| !f.is_dir).collect();
        if targets.is_empty() {
            self.notice("info", "请选择文件（文件夹没有直链）");
            return;
        }
        let Some(client) = self.client() else { return };
        let this = Arc::clone(self);
        self.backend.spawn(move || async move {
            let mut ok = 0usize;
            for f in targets {
                match client.get_download_url(&f.id).await {
                    Ok(url) => {
                        let copied = copy_to_clipboard(&url).is_ok();
                        this.push_link(f.name.clone(), url, copied);
                        ok += 1;
                    }
                    Err(e) => {
                        this.notice("error", format!("{} 获取直链失败：{e}", f.name));
                    }
                }
            }
            if ok > 0 {
                this.notice(
                    "success",
                    format!("已提取 {ok} 个下载直链，可在“下载直链”页复制"),
                );
                this.set_ui_state(|ui| ui.set_current_page(3));
            }
        });
    }

    fn push_link(self: &Arc<Self>, name: String, url: String, copied: bool) {
        {
            let mut links = lock_or_recover(&self.links);
            if let Some(exist) = links.iter_mut().find(|l| l.name == name) {
                exist.url = url.clone().into();
            } else {
                links.insert(
                    0,
                    DirectLink {
                        name: name.clone().into(),
                        url: url.clone().into(),
                    },
                );
                if links.len() > 50 {
                    links.truncate(50);
                }
            }
        }
        if copied {
            self.set_status(format!("{name} 的直链已复制到剪贴板"));
        }
        self.push_links();
    }

    fn push_links(&self) {
        let links: Vec<DirectLink> = lock_or_recover(&self.links).clone();
        self.set_ui_state(move |ui| {
            ui.set_links(slint::ModelRc::new(slint::VecModel::from(links)));
        });
    }

    pub(super) fn copy_link(&self, url: &str) {
        match copy_to_clipboard(url) {
            Ok(()) => self.notice("success", "直链已复制到剪贴板"),
            Err(e) => self.notice("error", format!("复制失败：{e}")),
        }
    }

    pub(super) fn clear_links(self: &Arc<Self>) {
        lock_or_recover(&self.links).clear();
        self.push_links();
        self.notice("info", "已清空直链列表");
    }

    /// 用系统默认浏览器打开一条直链
    pub(super) fn open_link(&self, url: &str) {
        match open_url_in_browser(url) {
            Ok(()) => self.set_status(format!("已在浏览器中打开：{url}")),
            Err(e) => self.notice("error", format!("无法在浏览器中打开：{e}")),
        }
    }

    /// 把直链列表的全部链接按行拼起来复制到剪贴板
    pub(super) fn copy_all_links(self: &Arc<Self>) {
        let links = lock_or_recover(&self.links).clone();
        let text = join_links(&links);
        if links.is_empty() {
            self.notice("info", "还没有可复制的直链");
            return;
        }
        match copy_to_clipboard(&text) {
            Ok(()) => self.notice("success", format!("已复制 {} 条直链", links.len())),
            Err(e) => self.notice("error", format!("复制失败：{e}")),
        }
    }

    pub(super) fn find_visible(&self, id: &str) -> Option<FileObject> {
        lock_or_recover(&self.visible_rows)
            .iter()
            .find(|f| f.id == id)
            .cloned()
    }

    // ==================================================================
    // 搜索
    // ==================================================================
    pub(super) fn search_changed(self: &Arc<Self>, keyword: &str) {
        let keyword = keyword.trim().to_string();
        if keyword.is_empty() {
            self.clear_search();
            return;
        }
        let gen = self.search_gen.fetch_add(1, Ordering::SeqCst) + 1;
        let Some(client) = self.client() else { return };
        let this = Arc::clone(self);
        self.set_selection(Vec::new());
        self.set_ui_state(|ui| {
            ui.set_searching(true);
        });
        self.set_list_state("loading", "搜索中…");
        // 去抖已在 UI 侧完成；这里只做乱序丢弃
        self.backend.spawn(move || async move {
            if this.search_gen.load(Ordering::SeqCst) != gen {
                return;
            }
            match client.search_files(&keyword).await {
                Ok(mut files) => {
                    if this.search_gen.load(Ordering::SeqCst) != gen {
                        return;
                    }
                    this.sorted_rows(&mut files);
                    let count = files.len();
                    *lock_or_recover(&this.visible_rows) = files.clone();
                    this.retain_selection(&files);
                    this.push_rows(&files);
                    this.set_ui_state(|ui| {
                        ui.set_searching(true);
                    });
                    this.set_list_state(
                        if count == 0 { "empty-search" } else { "ready" },
                        &format!("“{keyword}”的搜索结果：{count} 项"),
                    );
                }
                Err(e) => {
                    if this.search_gen.load(Ordering::SeqCst) != gen {
                        return;
                    }
                    let msg = format!("搜索失败：{e}");
                    this.set_file_error(msg.clone());
                    this.set_list_state("error", &msg);
                    this.notice("error", msg);
                }
            }
        });
    }

    pub(super) fn clear_search(self: &Arc<Self>) {
        // 让任何在途搜索失效，并恢复当前目录内容
        self.search_gen.fetch_add(1, Ordering::SeqCst);
        let dir = lock_or_recover(&self.dir_rows).clone();
        let rows = Self::rows_for_display(&dir, None);
        *lock_or_recover(&self.visible_rows) = rows.clone();
        self.retain_selection(&rows);
        let count = rows.len();
        self.push_rows(&rows);
        self.set_ui_state(|ui| {
            ui.set_searching(false);
            ui.set_search_keyword(SharedString::default());
        });
        self.set_list_state(
            if count == 0 { "empty" } else { "ready" },
            &format!("{count} 个项目"),
        );
    }

    // ==================================================================
    // 预览
    // ==================================================================
    fn preview_file(self: &Arc<Self>, id: &str, name: &str) {
        let kind = file::classify(name);
        let Some(client) = self.client() else { return };
        let id = id.to_string();
        let name = name.to_string();
        let this = Arc::clone(self);
        self.set_status(format!("正在打开 {name}…"));
        self.backend.spawn(move || async move {
            let url = match client.get_download_url(&id).await {
                Ok(u) => u,
                Err(e) => {
                    this.notice("error", format!("获取直链失败：{e}"));
                    return;
                }
            };
            // 音视频：交给系统播放器流式播放，不落盘
            if matches!(kind, FileKind::Video | FileKind::Audio) {
                match file::open_media_url_with_player(&name, &url) {
                    Ok(()) => this.set_status(format!("已用系统播放器打开 {name}")),
                    Err(e) => this.notice("error", format!("打开播放器失败：{e}")),
                }
                return;
            }
            // 其它类型：下载到临时目录后用系统默认程序打开
            match this.download_preview(&client, &url, &name).await {
                Ok(path) => {
                    if let Err(e) = file::open_with_system(&path) {
                        this.notice("error", format!("打开文件失败：{e}"));
                    } else {
                        this.set_status(format!("已打开 {name}"));
                    }
                }
                Err(e) => this.notice("error", format!("预览失败：{e}")),
            }
        });
    }

    async fn download_preview(
        &self,
        client: &TianyiClient,
        url: &str,
        name: &str,
    ) -> anyhow::Result<PathBuf> {
        let dir = std::env::temp_dir().join("tianyi-preview");
        tokio::fs::create_dir_all(&dir).await?;
        let path = dir.join(sanitize_file_name(name));
        if path.exists() {
            return Ok(path);
        }
        let resp = client.http_client().get(url).send().await?;
        let bytes = resp.bytes().await?;
        tokio::fs::write(&path, bytes).await?;
        Ok(path)
    }
}
/// 传输结果归一化
/// 需要用户确认后才执行的动作
///
/// 危险操作不直接执行：先落到这里，由 UI 的对话框回传确认或取消。
pub(super) enum PendingConfirm {
    DeleteFiles(Vec<FileObject>),
}

pub(super) fn files_to_entries(files: &[FileObject], selected: &HashSet<String>) -> Vec<FileEntry> {
    files
        .iter()
        .map(|f| {
            let kind = if f.is_dir {
                "folder"
            } else {
                match file::classify(&f.name) {
                    FileKind::Image => "image",
                    FileKind::Video => "video",
                    FileKind::Audio => "audio",
                    FileKind::Pdf => "pdf",
                    FileKind::Text => "text",
                    FileKind::Archive | FileKind::Torrent => "archive",
                    FileKind::Office => "office",
                    FileKind::Other => "other",
                }
            };
            let kind_text = if f.is_dir {
                "文件夹".to_string()
            } else {
                file::classify(&f.name).label().to_string()
            };
            FileEntry {
                id: f.id.clone().into(),
                name: f.name.clone().into(),
                size: f.size as i32,
                size_text: if f.is_dir {
                    "—".to_string()
                } else {
                    file::format_size(f.size.max(0) as u64)
                }
                .into(),
                is_dir: f.is_dir,
                modified: file::format_datetime(&f.last_op_time).into(),
                kind: kind.into(),
                kind_text: kind_text.into(),
                selected: selected.contains(&f.id),
            }
        })
        .collect()
}

/// Shift 范围选择：返回锚点到目标之间（含两端）的 id 列表，按显示顺序
///
/// 锚点或目标已不在当前列表中时返回 None，调用方退化为单选。
pub fn range_selection(rows: &[FileObject], anchor: &str, target: &str) -> Option<Vec<String>> {
    let from = rows.iter().position(|f| f.id == anchor)?;
    let to = rows.iter().position(|f| f.id == target)?;
    let (lo, hi) = if from <= to { (from, to) } else { (to, from) };
    Some(rows[lo..=hi].iter().map(|f| f.id.clone()).collect())
}

/// 排序键的中文名称（状态栏提示用）
fn sort_label(key: i32) -> &'static str {
    match key {
        1 => "大小",
        2 => "修改时间",
        _ => "名称",
    }
}

/// 取消加载后的列表状态归一
///
/// 有已就绪的行就回到 `ready`（不把刚到手的内容被"取消"藏起来），否则 `empty`
/// （交给空状态去说明这里没有内容）。
///
/// 抽成独立函数是为了能单测：`cancel_loading` 本身要碰 UI 状态与 `Arc<Controller>`，
/// 而"归一到哪个状态"是它唯一的纯逻辑分支。
fn cancelled_list_state(row_count: usize) -> &'static str {
    if row_count == 0 {
        "empty"
    } else {
        "ready"
    }
}

/// 按指定列排序：文件夹始终排在文件前面，组内按列比较
///
/// * 名称：忽略大小写比较，完全相同再按原始字节比较，保证顺序稳定
/// * 大小：数值比较
/// * 修改时间：沿用接口返回的原始时间串（`yyyy-MM-dd HH:mm:ss`），空值始终排在最后
pub fn sort_files(rows: &mut [FileObject], key: i32, asc: bool) {
    rows.sort_by(|a, b| {
        let dir_ord = b.is_dir.cmp(&a.is_dir); // 文件夹（true）在前
        if dir_ord != std::cmp::Ordering::Equal {
            return dir_ord;
        }
        let ord = match key {
            1 => a.size.cmp(&b.size),
            2 => match (a.last_op_time.is_empty(), b.last_op_time.is_empty()) {
                (true, true) => std::cmp::Ordering::Equal,
                (true, false) => return std::cmp::Ordering::Greater,
                (false, true) => return std::cmp::Ordering::Less,
                (false, false) => a.last_op_time.cmp(&b.last_op_time),
            },
            _ => {
                let la = a.name.to_lowercase();
                let lb = b.name.to_lowercase();
                la.cmp(&lb).then_with(|| a.name.cmp(&b.name))
            }
        };
        if asc {
            ord
        } else {
            ord.reverse()
        }
    });
}

fn entries_model(entries: Vec<FileEntry>) -> slint::ModelRc<FileEntry> {
    slint::ModelRc::new(slint::VecModel::from(entries))
}

/// 把一组行数据推到 UI 的 `file-list`，顺带更新选中数量。
///
/// **行集合没变时就地逐行更新**（复用同一个模型实例），只有行集合真的换了
/// （换目录 / 换云空间 / 搜索结果变化）才整表替换。
///
/// 为什么必须在乎模型实例的身份：Slint 的 `double-clicked` 依赖窗口里 `ClickState`
/// 的点击计数，而命中项（ItemRc）一变，运行时就把计数清零（i-slint-core `input.rs`
/// 的 `event.set_click_count(0)`）。整表换模型会让 ListView 的重排器重建所有行实例，
/// 第二下点击落在新实例上，双击因此永远凑不齐——用户看到的就是"双击打不开文件夹"。
/// 就地更新走 `row_changed`，行实例原地存活，双击判定才成立。
pub(crate) fn apply_entries(ui: &MainWindow, entries: Vec<FileEntry>, selected_count: i32) {
    use slint::Model;
    let model = ui.get_file_list();
    if same_row_ids(&model, &entries) {
        for (idx, entry) in entries.into_iter().enumerate() {
            model.set_row_data(idx, entry);
        }
    } else {
        ui.set_file_list(entries_model(entries));
    }
    ui.set_selected_count(selected_count);
}

/// 模型里的行 id 序列是否与 `entries` 完全一致（用于判断"能不能就地更新"）
fn same_row_ids(model: &slint::ModelRc<FileEntry>, entries: &[FileEntry]) -> bool {
    use slint::Model;
    model.row_count() == entries.len()
        && entries
            .iter()
            .enumerate()
            .all(|(idx, entry)| model.row_data(idx).is_some_and(|row| row.id == entry.id))
}

/// 去掉文件名中不安全的字符
pub(super) fn sanitize_file_name(name: &str) -> String {
    let cleaned: String = name
        .chars()
        .map(|c| match c {
            '/' | '\\' | ':' | '*' | '?' | '"' | '<' | '>' | '|' => '_',
            c if (c as u32) < 0x20 => '_',
            c => c,
        })
        .collect();
    if cleaned.trim().is_empty() {
        "unnamed".to_string()
    } else {
        cleaned
    }
}

/// 取消重命名后要写回 UI 的 (renaming-id, 状态栏文案)
///
/// 抽成纯函数，让"取消之后 renaming-id 必定为空"这条约定能被单元测试钉住：
/// 重命名输入框原本只有 accepted 一个出口，Esc 必须走同一条清空路径。
pub(super) fn rename_cancelled_state() -> (SharedString, &'static str) {
    (SharedString::default(), "已取消重命名")
}

/// 把直链列表拼成一段多行文本（每行一条，无尾随换行）
///
/// 0 条 → 空串；N 条 → 用 `\n` 连接。`\n` 而不是 `\r\n`：粘贴到下载工具
/// （IDM / aria2 / Motrix 等）时两种都认，而 `\r` 在部分工具里会被当成链接的一部分。
pub(super) fn join_links(links: &[DirectLink]) -> String {
    links
        .iter()
        .map(|link| link.url.as_str())
        .collect::<Vec<_>>()
        .join("\n")
}

/// 只放行 http(s)：直链一定是这两者，其它 scheme 交给系统打开只会扩大攻击面
fn is_openable_url(url: &str) -> bool {
    let url = url.trim();
    url.starts_with("http://") || url.starts_with("https://")
}

/// 用系统默认浏览器打开一个 URL
///
/// 不能复用 `tianyi_core::file::open_with_system`：它按“文件路径”处理参数，Windows 上
/// 会把 `/` 换成 `\`（`https://host/a/b` → `https:\\host\a\b`），URL 会打不开。这里按
/// “URL 交给系统打开程序”单独实现，且刻意放在本层，不去改 core 的既有行为。
fn open_url_in_browser(url: &str) -> Result<(), String> {
    if !is_openable_url(url) {
        return Err(format!("不是可用的直链：{url}"));
    }

    #[cfg(target_os = "windows")]
    {
        use std::os::windows::process::CommandExt;
        // 直链普遍带 `&`（查询串），cmd 会把它当成命令分隔符，所以整条 URL 必须显式
        // 加引号；`start` 又会把第一个带引号的参数当作窗口标题，因此先给一个空标题。
        let quoted = format!("\"{}\"", url.trim().replace('"', "%22"));
        let mut cmd = std::process::Command::new("cmd");
        cmd.args(["/C", "start", ""]).raw_arg(quoted);
        cmd.spawn()
            .map(|_| ())
            .map_err(|e| format!("launch browser: {e}"))
    }

    #[cfg(target_os = "macos")]
    {
        std::process::Command::new("open")
            .arg(url)
            .spawn()
            .map(|_| ())
            .map_err(|e| format!("launch browser: {e}"))
    }

    #[cfg(target_os = "linux")]
    {
        std::process::Command::new("xdg-open")
            .arg(url)
            .spawn()
            .map(|_| ())
            .map_err(|e| format!("launch browser: {e}"))
    }

    #[cfg(not(any(target_os = "windows", target_os = "macos", target_os = "linux")))]
    {
        Err("unsupported platform".to_string())
    }
}

#[cfg(test)]
mod link_tests {
    use super::*;

    fn link(name: &str, url: &str) -> DirectLink {
        DirectLink {
            name: name.into(),
            url: url.into(),
        }
    }

    #[test]
    fn join_links_is_empty_when_there_is_nothing_to_copy() {
        assert_eq!(join_links(&[]), "");
    }

    #[test]
    fn join_links_joins_with_single_newline_and_no_trailing_break() {
        let links = [
            link("a.bin", "https://x.test/1?a=1&b=2"),
            link("b.bin", "https://x.test/2"),
        ];
        assert_eq!(
            join_links(&links),
            "https://x.test/1?a=1&b=2\nhttps://x.test/2"
        );
        assert!(!join_links(&links).ends_with('\n'), "不能有尾随换行");
        assert!(!join_links(&links).contains('\r'), "不能用 CRLF");
    }

    #[test]
    fn is_openable_url_accepts_only_http_and_https() {
        assert!(is_openable_url("https://x.test/a"));
        assert!(is_openable_url("http://x.test/a?b=1&c=2"));
        // 前后空白被忽略（从剪贴板/配置里粘贴回来的常见形态）
        assert!(is_openable_url("  https://x.test/a  "));
        assert!(!is_openable_url(""));
        assert!(!is_openable_url("   "));
        assert!(!is_openable_url("file:///C:/Windows/win.ini"));
        assert!(!is_openable_url("javascript:alert(1)"));
        assert!(!is_openable_url("ftp://x.test/a"));
    }

    #[test]
    fn open_link_rejects_non_http_url_without_touching_the_system() {
        // 非法 scheme 必须在启动任何进程之前就被拦下（这里能返回 Err 即为证明）
        assert!(open_url_in_browser("javascript:alert(1)").is_err());
    }
}

#[cfg(test)]
mod cancel_tests {
    use super::*;

    /// 取消不是"清空"，而是"把状态归一到当前真有的内容"。
    #[test]
    fn cancel_loading_normalizes_list_state_by_row_count() {
        assert_eq!(cancelled_list_state(0), "empty");
        assert_eq!(cancelled_list_state(1), "ready");
        assert_eq!(cancelled_list_state(8), "ready");
    }
}

#[cfg(test)]
mod space_tests {
    use super::*;

    /// 空操作只发生在「同一空间 + 已在根目录 + 非搜索态」这一种组合上。
    ///
    /// 反过来说：另外三条分支里任何一条成立，都必须真切换——尤其是
    /// 「同一空间但在子目录」，那是用户点导航项回根目录的正常路径，
    /// 被一起挡掉就会变成"点了没反应，还停在子目录里"。
    #[test]
    fn switch_space_is_a_noop_only_at_the_same_space_root() {
        // (当前空间, 已在根目录, 搜索态, 目标空间, 期望"必须切换")
        let cases = [
            ("personal", true, false, "personal", false), // 根目录再点一次 = 空操作
            ("family", true, false, "family", false),
            ("personal", true, true, "personal", true), // 搜索态要恢复成目录列表
            ("personal", false, false, "personal", true), // 子目录里点回根目录
            ("family", false, false, "family", true),
            ("personal", true, false, "family", true), // 换空间永远要切换
            ("family", true, false, "personal", true),
            ("personal", false, true, "family", true),
        ];
        for (cur, at_root, searching, target, want) in cases {
            assert_eq!(
                Controller::should_switch_space(cur, at_root, searching, target),
                want,
                "current={cur} at_root={at_root} searching={searching} target={target}"
            );
        }
    }
}
