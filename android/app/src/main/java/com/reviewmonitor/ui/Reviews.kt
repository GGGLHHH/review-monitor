package com.reviewmonitor.ui

import android.graphics.Bitmap
import android.graphics.BitmapFactory
import android.util.LruCache
import androidx.compose.foundation.Image
import androidx.compose.foundation.background
import androidx.compose.foundation.border
import androidx.compose.foundation.clickable
import androidx.compose.foundation.horizontalScroll
import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Box
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.PaddingValues
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.Spacer
import androidx.compose.foundation.layout.fillMaxSize
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.imePadding
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.layout.size
import androidx.compose.foundation.layout.width
import androidx.compose.foundation.lazy.LazyColumn
import androidx.compose.foundation.lazy.LazyRow
import androidx.compose.foundation.lazy.items
import androidx.compose.foundation.lazy.itemsIndexed
import androidx.compose.foundation.lazy.rememberLazyListState
import androidx.compose.foundation.gestures.awaitEachGesture
import androidx.compose.foundation.gestures.awaitFirstDown
import androidx.compose.foundation.gestures.calculatePan
import androidx.compose.foundation.gestures.calculateZoom
import androidx.compose.foundation.gestures.detectTapGestures
import androidx.compose.foundation.pager.HorizontalPager
import androidx.compose.foundation.pager.rememberPagerState
import androidx.compose.animation.AnimatedVisibility
import androidx.compose.animation.core.animateFloatAsState
import androidx.compose.animation.core.spring
import androidx.compose.animation.core.tween
import androidx.compose.animation.expandVertically
import androidx.compose.animation.fadeIn
import androidx.compose.animation.shrinkVertically
import androidx.compose.animation.fadeOut
import androidx.compose.animation.scaleIn
import androidx.compose.animation.scaleOut
import androidx.compose.foundation.interaction.MutableInteractionSource
import androidx.compose.foundation.interaction.collectIsPressedAsState
import androidx.compose.foundation.shape.CircleShape
import androidx.compose.runtime.derivedStateOf
import androidx.compose.ui.draw.shadow
import androidx.compose.runtime.mutableFloatStateOf
import androidx.compose.runtime.snapshotFlow
import androidx.compose.ui.geometry.Offset
import androidx.compose.ui.graphics.Color
import androidx.compose.ui.graphics.graphicsLayer
import androidx.compose.ui.input.pointer.pointerInput
import androidx.compose.ui.input.pointer.positionChanged
import androidx.compose.ui.layout.onSizeChanged
import androidx.compose.ui.unit.IntSize
import androidx.compose.ui.window.Dialog
import androidx.compose.ui.window.DialogProperties
import kotlinx.coroutines.flow.distinctUntilChanged
import kotlinx.coroutines.flow.filter
import androidx.compose.foundation.rememberScrollState
import androidx.compose.foundation.shape.RoundedCornerShape
import androidx.compose.material3.Text
import androidx.compose.runtime.Composable
import androidx.compose.runtime.LaunchedEffect
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableIntStateOf
import androidx.compose.runtime.mutableStateListOf
import androidx.compose.runtime.mutableStateMapOf
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.produceState
import androidx.compose.runtime.remember
import androidx.compose.runtime.rememberCoroutineScope
import androidx.compose.runtime.saveable.rememberSaveable
import androidx.compose.runtime.setValue
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.draw.clip
import androidx.compose.ui.graphics.ImageBitmap
import androidx.compose.ui.graphics.asImageBitmap
import androidx.compose.ui.layout.ContentScale
import androidx.compose.ui.text.font.FontWeight
import androidx.compose.ui.unit.dp
import androidx.compose.ui.unit.sp
import com.reviewmonitor.core.Engine
import com.reviewmonitor.core.EngineException
import com.reviewmonitor.core.FeedbackInfo
import com.reviewmonitor.core.ReplyInfo
import com.reviewmonitor.core.ReviewQuery
import com.reviewmonitor.core.reviewFilters
import com.reviewmonitor.core.reviewStarOptions
import kotlinx.coroutines.delay
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.Job
import kotlinx.coroutines.launch
import kotlinx.coroutines.withContext
import java.net.HttpURLConnection
import java.net.URL

/**
 * 评价页：照经营宝网页版的筛选条件直接查接口（Rust 带着登录 cookie 发，不经过网页）。
 * 每次改筛选从第 1 页重新查，「加载更多」往后翻。只读，这一页不改任何东西。
 */
/** 要在评价页定位的那一条（点差评通知进来）；nonce 让同一条点两次也能再定位 */
data class Focus(val key: String, val nonce: Long)

@Composable
fun ReviewsPane(engine: Engine?, focus: Focus? = null) {
    var isMt by rememberSaveable { mutableStateOf(true) }
    var highlight by rememberSaveable { mutableStateOf("") }
    val listState = rememberLazyListState()
    // 两个平台各记各的筛选，切回来还是原来选的
    val picked = remember { mutableStateMapOf<String, String>() }
    val groups = remember(isMt) { reviewFilters(isMt) }
    fun valueOf(key: String, def: String) = picked["${if (isMt) "mt" else "dp"}.$key"] ?: def
    fun filters() = groups.associate { it.key to valueOf(it.key, it.defaultValue) }
    // 按「几星」细筛（网页没有；Rust 换成最窄的星级档位去查，拉回来再筛）。只认当前档位里有的星数
    val stars = remember { mutableStateMapOf<String, List<Int>>() }
    val band = valueOf("starType", "全部")
    val starOpts = remember(isMt, band) { reviewStarOptions(isMt, band).map { it.toInt() } }
    fun pickedStars() = stars[if (isMt) "mt" else "dp"].orEmpty().filter { it in starOpts }.sorted()

    val items = remember { mutableStateListOf<FeedbackInfo>() }
    var total by remember { mutableIntStateOf(0) }
    var page by remember { mutableIntStateOf(0) }
    // 接口实际翻过多少条；细筛时显示的条数会比它少，翻页按它算
    var scanned by remember { mutableIntStateOf(0) }
    // 细筛时连续多少页一条都没筛中：太多就停下等用户点，免得一口气把几百条翻完
    var dry by remember { mutableIntStateOf(0) }
    var loading by remember { mutableStateOf(false) }
    var error by remember { mutableStateOf("") }
    var reload by remember { mutableIntStateOf(0) }
    val scope = rememberCoroutineScope()
    var job by remember { mutableStateOf<Job?>(null) }
    // 往下滑后筛选区收成一行摘要；点摘要临时展开
    var filtersOpen by remember { mutableStateOf(false) }

    fun load(next: Int) {
        val e = engine ?: return
        val q = ReviewQuery(isMt, next.toUInt(), filters(), pickedStars().map { it.toByte() }.toByteArray())
        job?.cancel()
        loading = true; error = ""
        if (next == 1) { items.clear(); total = 0; page = 0; scanned = 0; dry = 0; filtersOpen = false }
        val pause = next > 1 && dry > 0
        job = scope.launch {
            // 上一页没筛中、马上又要翻：稍停一下，别连珠炮似的打接口
            if (pause) delay(400)
            val r = withContext(Dispatchers.IO) { runCatching { e.fetchReviews(q) } }
            r.onSuccess { p ->
                if (next == 1) items.clear()
                val fresh = p.items.filter { n -> items.none { it.key == n.key } }
                items.addAll(fresh)
                total = p.total.toInt(); page = next
                // 接口返回空页就当到底了，免得一直翻
                scanned = if (p.scanned == 0u) total else scanned + p.scanned.toInt()
                dry = if (fresh.isEmpty()) dry + 1 else 0
            }.onFailure { ex -> error = (ex as? EngineException.Failed)?.msg ?: (ex.message ?: "查询失败") }
            loading = false
        }
    }
    // 点通知进来：切到那个平台，看近 7 天的全部评价（新差评一定在第一页），定位并高亮那一条
    LaunchedEffect(focus) {
        val f = focus ?: return@LaunchedEffect
        val mt = f.key.startsWith("mt:")
        val p = if (mt) "mt" else "dp"
        isMt = mt
        for (g in reviewFilters(mt)) picked["$p.${g.key}"] = if (g.key == "timeType") "近7天" else "全部"
        stars.remove(p)
        highlight = f.key
        reload++
    }
    LaunchedEffect(highlight, items.size) {
        val i = items.indexOfFirst { it.key == highlight }
        // 顶部（平台、筛选、条数）固定在列表外面，列表里第 i 项就是第 i 条评价
        if (i >= 0) listState.animateScrollToItem(i)
    }

    val sig = filters().toString() + pickedStars()
    val more = page > 0 && scanned < total
    val paused = dry >= DRY_PAGES
    LaunchedEffect(engine, isMt, sig, reload) { load(1) }
    // 滑到快到底（还剩 4 项）就自动拉下一页；出错时停下，等用户点「重试」
    LaunchedEffect(listState, engine) {
        snapshotFlow {
            val li = listState.layoutInfo
            val nearEnd = (li.visibleItemsInfo.lastOrNull()?.index ?: 0) >= li.totalItemsCount - 4
            nearEnd && page > 0 && scanned < total && !loading && error.isEmpty() && dry < DRY_PAGES
        }.distinctUntilChanged().filter { it }.collect { load(page + 1) }
    }
    // 点缩略图：全屏看这条评价的大图
    var viewer by remember { mutableStateOf<Pair<List<String>, Int>?>(null) }
    viewer?.let { (urls, start) -> ImageViewer(urls, start) { viewer = null } }

    val scrolled by remember { derivedStateOf { listState.canScrollBackward } }
    val showFilters = !scrolled || filtersOpen
    // 回到顶部就清掉「手动展开」，下次往下滑照常收起
    LaunchedEffect(scrolled) { if (!scrolled) filtersOpen = false }
    Column(Modifier.fillMaxSize()) {
    // 顶部固定：平台 + 刷新、筛选（滑动后收成摘要）、条数
    Column(
        Modifier.fillMaxWidth().background(C.bg).padding(start = 16.dp, end = 16.dp, top = 16.dp, bottom = 10.dp),
        verticalArrangement = Arrangement.spacedBy(10.dp),
    ) {
        Row(verticalAlignment = Alignment.CenterVertically) {
            Seg(listOf("美团", "点评"), if (isMt) 0 else 1) { isMt = it == 0 }
            Spacer(Modifier.weight(1f))
            Pill("刷新", enabled = !loading) { reload++ }
        }
        if (scrolled) {
            val parts = groups.map { valueOf(it.key, it.defaultValue) }.filter { it != "全部" } +
                pickedStars().joinToString("") { "$it★" }.let { if (it.isEmpty()) emptyList() else listOf(it) }
            Row(
                Modifier.fillMaxWidth().clip(RoundedCornerShape(10.dp)).background(C.card).border(1.dp, C.border, RoundedCornerShape(10.dp))
                    .clickable { filtersOpen = !filtersOpen }.padding(horizontal = 12.dp, vertical = 9.dp),
                verticalAlignment = Alignment.CenterVertically,
            ) {
                Text("筛选", color = C.faint, fontSize = 12.sp, modifier = Modifier.width(44.dp))
                Text(parts.ifEmpty { listOf("全部评价") }.joinToString(" · "), color = C.accent, fontSize = 13.sp,
                    maxLines = 1, modifier = Modifier.weight(1f))
                Text(if (filtersOpen) "收起 ▴" else "展开 ▾", color = C.sub, fontSize = 12.sp)
            }
        }
        AnimatedVisibility(showFilters, enter = expandVertically(tween(220)) + fadeIn(tween(220)), exit = shrinkVertically(tween(200)) + fadeOut(tween(160))) {
            Column(
                Modifier.fillMaxWidth().clip(RoundedCornerShape(10.dp)).background(C.card).border(1.dp, C.border, RoundedCornerShape(10.dp))
                    .padding(vertical = 6.dp),
            ) {
                for (g in groups) {
                    FilterRow(g.label, g.options, valueOf(g.key, g.defaultValue)) { v -> picked["${if (isMt) "mt" else "dp"}.${g.key}"] = v }
                    if (g.key == "starType" && starOpts.size > 1) StarRow(starOpts, pickedStars()) { n ->
                        val pf = if (isMt) "mt" else "dp"
                        val cur = pickedStars()
                        stars[pf] = if (n in cur) cur - n else cur + n
                    }
                }
            }
        }
        val head = when {
            loading && items.isEmpty() -> "正在查询…"
            error.isNotEmpty() && items.isEmpty() -> ""
            pickedStars().isNotEmpty() ->
                "找到 ${items.size} 条" + (if (scanned < total) "，已翻 $scanned / $total 条" else "（共翻 $total 条）") + " · 半星算低一档"
            else -> "共 $total 条" + if (items.size < total) "，已显示 ${items.size} 条" else ""
        }
        if (head.isNotEmpty()) Text(head, color = C.sub, fontSize = 13.sp)
    }
    // 滑动后顶部和列表之间画一条分隔线
    if (scrolled) Box(Modifier.fillMaxWidth().size(height = 1.dp, width = 0.dp).background(C.border))
    Box(Modifier.weight(1f).fillMaxWidth()) {
    LazyColumn(Modifier.fillMaxSize().imePadding(), state = listState, contentPadding = PaddingValues(start = 16.dp, end = 16.dp, top = 6.dp, bottom = 16.dp), verticalArrangement = Arrangement.spacedBy(12.dp)) {
        // 第一页就失败：错误放上面；往后翻页失败放在列表底部
        if (error.isNotEmpty() && items.isEmpty()) item { Note(error, C.bad) }
        items(items, key = { it.key }) { f ->
            ReviewCard(f, engine, highlighted = f.key == highlight, onOpenPicture = { i -> viewer = f.picturesFull.ifEmpty { f.pictures } to i }) { content ->
                // 发成功了：就地标成已回复并显示这条回复，不必重新查
                val i = items.indexOfFirst { it.key == f.key }
                if (i >= 0) items[i] = f.copy(replied = true, replies = f.replies + ReplyInfo(0, 0, true, "商家", content, "刚刚"))
            }
        }
        if (!loading && error.isEmpty() && page > 0 && !more && items.isEmpty()) item {
            Text("没有符合条件的评价", color = C.faint, fontSize = 14.sp, modifier = Modifier.fillMaxWidth().padding(vertical = 24.dp), textAlign = androidx.compose.ui.text.style.TextAlign.Center)
        }
        if (page > 0 && (items.isNotEmpty() || more)) item {
            Column(Modifier.fillMaxWidth().padding(vertical = 8.dp), horizontalAlignment = Alignment.CenterHorizontally, verticalArrangement = Arrangement.spacedBy(8.dp)) {
                when {
                    !more -> Text("已经到底了", color = C.faint, fontSize = 12.sp)
                    error.isNotEmpty() -> { Note(error, C.bad); Pill("重试") { load(page + 1) } }
                    paused && !loading -> {
                        Text("连翻 $DRY_PAGES 页都没有符合的，先停一下", color = C.faint, fontSize = 12.sp)
                        Pill("继续往下找") { dry = 0; load(page + 1) }
                    }
                    else -> Text(if (pickedStars().isNotEmpty() && items.isEmpty()) "正在往下找…" else "正在加载…", color = C.faint, fontSize = 12.sp)
                }
            }
        }
    }
    // 滑过筛选区后出现「回到顶部」：淡入 + 放大出现，点了平滑滚回去
    val showTop by remember { derivedStateOf { listState.firstVisibleItemIndex > 1 } }
    androidx.compose.animation.AnimatedVisibility(
        visible = showTop,
        enter = fadeIn(tween(200)) + scaleIn(tween(220), initialScale = 0.6f),
        exit = fadeOut(tween(180)) + scaleOut(tween(180), targetScale = 0.6f),
        modifier = Modifier.align(Alignment.BottomEnd).padding(end = 20.dp, bottom = 24.dp),
    ) {
        ToTopButton { scope.launch { listState.animateScrollToItem(0) } }
    }
    }
    }
}

/** 细筛时连续这么多页没筛中就停下，等用户点「继续往下找」 */
private const val DRY_PAGES = 10

/** 「细分」一行：1★~5★ 多选，不选 = 不细筛 */
@Composable
private fun StarRow(options: List<Int>, picked: List<Int>, onToggle: (Int) -> Unit) {
    Row(Modifier.fillMaxWidth().padding(horizontal = 12.dp, vertical = 5.dp), verticalAlignment = Alignment.CenterVertically) {
        Text("细分", color = C.faint, fontSize = 12.sp, modifier = Modifier.width(56.dp))
        Row(Modifier.horizontalScroll(rememberScrollState()), horizontalArrangement = Arrangement.spacedBy(6.dp)) {
            for (n in options) {
                val on = n in picked
                Box(
                    Modifier.clip(RoundedCornerShape(14.dp))
                        .background(if (on) C.accent.copy(alpha = 0.18f) else C.panel)
                        .border(1.dp, if (on) C.accent else C.border, RoundedCornerShape(14.dp))
                        .clickable { onToggle(n) }.padding(horizontal = 12.dp, vertical = 5.dp),
                ) { Text("$n★", color = if (on) C.accent else C.sub, fontSize = 13.sp, fontWeight = if (on) FontWeight.SemiBold else FontWeight.Normal) }
            }
        }
    }
}

/** 圆形悬浮按钮，按下时缩一下再弹回 */
@Composable
private fun ToTopButton(onClick: () -> Unit) {
    val src = remember { MutableInteractionSource() }
    val pressed by src.collectIsPressedAsState()
    val s by animateFloatAsState(if (pressed) 0.88f else 1f, spring(dampingRatio = 0.5f, stiffness = 600f), label = "toTop")
    Box(
        Modifier.size(52.dp).graphicsLayer(scaleX = s, scaleY = s)
            .shadow(8.dp, CircleShape).clip(CircleShape).background(C.accent)
            .clickable(interactionSource = src, indication = null, onClick = onClick),
        contentAlignment = Alignment.Center,
    ) { Text("↑", color = C.accentInk, fontSize = 24.sp, fontWeight = FontWeight.Bold) }
}

/** 两三个选项的分段按钮 */
@Composable
fun Seg(options: List<String>, selected: Int, onSelect: (Int) -> Unit) {
    Row(Modifier.clip(RoundedCornerShape(18.dp)).background(C.card).border(1.dp, C.border, RoundedCornerShape(18.dp)).padding(3.dp)) {
        options.forEachIndexed { i, t ->
            val on = i == selected
            Box(
                Modifier.clip(RoundedCornerShape(15.dp)).background(if (on) C.accent else C.card)
                    .clickable { onSelect(i) }.padding(horizontal = 18.dp, vertical = 7.dp),
            ) { Text(t, color = if (on) C.accentInk else C.sub, fontSize = 14.sp, fontWeight = if (on) FontWeight.Bold else FontWeight.Medium) }
        }
    }
}

/** 一组筛选：左边名字，右边一排可横向滑动的选项 */
@Composable
private fun FilterRow(label: String, options: List<String>, value: String, onPick: (String) -> Unit) {
    Row(Modifier.fillMaxWidth().padding(horizontal = 12.dp, vertical = 5.dp), verticalAlignment = Alignment.CenterVertically) {
        Text(label, color = C.faint, fontSize = 12.sp, modifier = Modifier.width(56.dp))
        Row(Modifier.horizontalScroll(rememberScrollState()), horizontalArrangement = Arrangement.spacedBy(6.dp)) {
            for (o in options) {
                val on = o == value
                Box(
                    Modifier.clip(RoundedCornerShape(14.dp))
                        .background(if (on) C.accent.copy(alpha = 0.18f) else C.panel)
                        .border(1.dp, if (on) C.accent else C.border, RoundedCornerShape(14.dp))
                        .clickable { onPick(o) }.padding(horizontal = 12.dp, vertical = 5.dp),
                ) { Text(o, color = if (on) C.accent else C.sub, fontSize = 13.sp, fontWeight = if (on) FontWeight.SemiBold else FontWeight.Normal) }
            }
        }
    }
}

@Composable
private fun ReviewCard(f: FeedbackInfo, engine: Engine?, highlighted: Boolean = false, onOpenPicture: (Int) -> Unit, onReplied: (String) -> Unit) {
    val bad = f.starValue <= 3.0f
    var editing by rememberSaveable(f.key) { mutableStateOf(false) }
    var text by rememberSaveable(f.key) { mutableStateOf("") }
    var confirm by remember { mutableStateOf(false) }
    var sending by remember { mutableStateOf(false) }
    var err by remember { mutableStateOf("") }
    val scope = rememberCoroutineScope()
    val where = if (f.isMt) "美团" else "大众点评"
    if (confirm) {
        androidx.compose.material3.AlertDialog(
            onDismissRequest = { confirm = false },
            containerColor = C.panel, titleContentColor = C.text, textContentColor = C.sub,
            title = { Text("发送这条回复？") },
            text = {
                Column(verticalArrangement = Arrangement.spacedBy(10.dp)) {
                    Text("回复会公开显示在$where 上，所有人都能看到。发错了可以到经营宝网页版删除。", fontSize = 13.sp, lineHeight = 19.sp)
                    Box(Modifier.fillMaxWidth().clip(RoundedCornerShape(8.dp)).background(C.card).padding(10.dp)) {
                        Text(text.trim(), color = C.text, fontSize = 14.sp, lineHeight = 20.sp)
                    }
                }
            },
            confirmButton = {
                Pill("发送", primary = true) {
                    confirm = false
                    val e = engine ?: return@Pill
                    val content = text.trim()
                    sending = true; err = ""
                    scope.launch {
                        val r = withContext(Dispatchers.IO) { runCatching { e.replyReview(f.key, content) } }
                        sending = false
                        r.onSuccess { text = ""; editing = false; onReplied(content) }
                            .onFailure { ex -> err = (ex as? EngineException.Failed)?.msg ?: (ex.message ?: "发送失败") }
                    }
                }
            },
            dismissButton = { Pill("再改改") { confirm = false } },
        )
    }
    Column(
        Modifier.fillMaxWidth().clip(RoundedCornerShape(10.dp)).background(C.card)
            .border(if (highlighted) 2.dp else 1.dp, if (highlighted) C.accent else C.border, RoundedCornerShape(10.dp))
            .padding(14.dp),
        verticalArrangement = Arrangement.spacedBy(8.dp),
    ) {
        if (highlighted) Text("通知里的这条", color = C.accent, fontSize = 12.sp, fontWeight = FontWeight.SemiBold)
        Row(verticalAlignment = Alignment.CenterVertically) {
            Text(f.star, color = if (bad) C.bad else C.good, fontSize = 16.sp, fontWeight = FontWeight.Bold)
            Spacer(Modifier.width(8.dp))
            Text(f.userName, color = C.text, fontSize = 14.sp, modifier = Modifier.weight(1f), maxLines = 1)
            Text(f.time, color = C.faint, fontSize = 12.sp)
        }
        Row(horizontalArrangement = Arrangement.spacedBy(6.dp)) {
            if (f.replied) Tag("已回复", C.good) else Tag("未回复", C.warn)
            if (f.returnedCustomer) Tag("回头客", C.accent)
            if (f.source.isNotEmpty()) Tag(f.source, C.sub)
        }
        if (f.subScores.isNotEmpty()) Text(f.subScores, color = C.faint, fontSize = 12.sp)
        if (f.dealName.isNotEmpty()) Text("团购：${f.dealName}", color = C.faint, fontSize = 12.sp)
        Text(
            f.content.ifEmpty { "（只打了分，没写内容）" },
            color = if (f.content.isEmpty()) C.faint else C.text, fontSize = 14.sp, lineHeight = 21.sp,
        )
        if (f.pictures.isNotEmpty()) {
            LazyRow(horizontalArrangement = Arrangement.spacedBy(6.dp)) {
                itemsIndexed(f.pictures) { i, u -> Thumb(u) { onOpenPicture(i) } }
            }
        }
        if (f.videoCount > 0u) Text("含 ${f.videoCount} 个视频（在美团 / 点评里看）", color = C.faint, fontSize = 12.sp)
        if (f.replies.isNotEmpty()) {
            Column(
                Modifier.fillMaxWidth().clip(RoundedCornerShape(8.dp)).background(C.panel).padding(10.dp),
                verticalArrangement = Arrangement.spacedBy(8.dp),
            ) { for (r in f.replies) ReplyLine(r) }
        }
        if (!f.replied) {
            if (!editing) {
                Row { Spacer(Modifier.weight(1f)); Pill("回复") { editing = true } }
            } else {
                ReplyBox(text, enabled = !sending, onChange = { text = it; err = "" })
                val n = text.trim().let { it.codePointCount(0, it.length) }
                Row(verticalAlignment = Alignment.CenterVertically) {
                    Text(
                        if (n > REPLY_MAX) "超出 ${n - REPLY_MAX} 字（最多 $REPLY_MAX 字）" else "$n / $REPLY_MAX 字",
                        color = if (n > REPLY_MAX) C.bad else C.faint, fontSize = 12.sp, modifier = Modifier.weight(1f),
                    )
                    Pill("取消", enabled = !sending) { editing = false; err = "" }
                    Spacer(Modifier.width(8.dp))
                    Pill(if (sending) "发送中…" else "发送", primary = true, enabled = !sending && n in 1..REPLY_MAX && engine != null) { confirm = true }
                }
            }
            if (err.isNotEmpty()) Note(err, C.bad)
        }
    }
}

/** 网页版回复框的上限（Rust 那边也会再校验一次） */
private const val REPLY_MAX = 500

@Composable
private fun ReplyBox(value: String, enabled: Boolean, onChange: (String) -> Unit) {
    androidx.compose.material3.OutlinedTextField(
        value = value, onValueChange = onChange, enabled = enabled,
        placeholder = { Text("写给顾客的回复…", color = C.faint) },
        minLines = 3, maxLines = 8,
        shape = RoundedCornerShape(10.dp),
        colors = androidx.compose.material3.OutlinedTextFieldDefaults.colors(
            focusedTextColor = C.text, unfocusedTextColor = C.text, disabledTextColor = C.sub,
            focusedContainerColor = C.panel, unfocusedContainerColor = C.panel, disabledContainerColor = C.panel,
            focusedBorderColor = C.accent, unfocusedBorderColor = C.border, disabledBorderColor = C.border,
            cursorColor = C.accent,
        ),
        modifier = Modifier.fillMaxWidth(),
    )
}

@Composable
private fun ReplyLine(r: ReplyInfo) {
    Column(verticalArrangement = Arrangement.spacedBy(2.dp)) {
        Row {
            Text(if (r.fromMerchant) "商家回复" else "${r.name}：", color = if (r.fromMerchant) C.accent else C.sub, fontSize = 12.sp, fontWeight = FontWeight.SemiBold)
            Spacer(Modifier.weight(1f))
            Text(r.time, color = C.faint, fontSize = 11.sp)
        }
        Text(r.content, color = C.sub, fontSize = 13.sp, lineHeight = 19.sp)
    }
}

// ---------------- 图片（公开 CDN，不带 cookie）----------------

/** 缩略图和大图分开缓存：列表滑动时不会把正在看的大图挤掉 */
private object Images {
    private fun cache(mb: Int) = object : LruCache<String, ImageBitmap>(mb * 1024 * 1024) {
        override fun sizeOf(key: String, value: ImageBitmap) = value.width * value.height * 4
    }
    val thumbs = cache(24)
    val full = cache(64)

    /** 下载并按 maxSide 缩小解码（只取 2 的幂次的采样，省内存） */
    fun load(url: String, big: Boolean): ImageBitmap? {
        val c = if (big) full else thumbs
        c.get(url)?.let { return it }
        return runCatching {
            val conn = URL(url).openConnection() as HttpURLConnection
            conn.connectTimeout = 8000; conn.readTimeout = if (big) 15000 else 8000
            val bytes = try { conn.inputStream.use { it.readBytes() } } finally { conn.disconnect() }
            val o = BitmapFactory.Options().apply { inJustDecodeBounds = true }
            BitmapFactory.decodeByteArray(bytes, 0, bytes.size, o)
            var s = 1
            if (big) { while (maxOf(o.outWidth, o.outHeight) / s > 2048) s *= 2 }
            else { while (o.outWidth / (s * 2) >= 240 && o.outHeight / (s * 2) >= 240) s *= 2 }
            val bmp: Bitmap = BitmapFactory.decodeByteArray(bytes, 0, bytes.size, BitmapFactory.Options().apply { inSampleSize = s }) ?: return null
            bmp.asImageBitmap().also { c.put(url, it) }
        }.onFailure { android.util.Log.w("RM.Img", "图片加载失败 ${url.substringBefore('?').take(80)}: $it") }.getOrNull()
    }
}

@Composable
private fun Thumb(url: String, onClick: () -> Unit) {
    val img by produceState(Images.thumbs.get(url), url) { if (value == null) value = withContext(Dispatchers.IO) { Images.load(url, big = false) } }
    Box(
        Modifier.size(76.dp).clip(RoundedCornerShape(6.dp)).background(C.panel).clickable(onClick = onClick),
        contentAlignment = Alignment.Center,
    ) {
        val b = img
        if (b != null) Image(b, null, Modifier.fillMaxSize(), contentScale = ContentScale.Crop)
        else Text("图", color = C.faint, fontSize = 12.sp)
    }
}

/**
 * 全屏看大图：左右滑切换同一条评价里的图片；双指缩放、双击放大 / 还原，放大后拖动是平移（不会误翻页）；
 * 单击或返回键关闭。
 */
@Composable
private fun ImageViewer(urls: List<String>, start: Int, onClose: () -> Unit) {
    Dialog(onDismissRequest = onClose, properties = DialogProperties(usePlatformDefaultWidth = false)) {
        val pager = rememberPagerState(initialPage = start.coerceIn(0, (urls.size - 1).coerceAtLeast(0))) { urls.size }
        Box(Modifier.fillMaxSize().background(Color.Black)) {
            HorizontalPager(pager, Modifier.fillMaxSize(), beyondViewportPageCount = 1, key = { urls[it] }) { i ->
                ZoomImage(urls[i], onTap = onClose)
            }
            Text(
                "${pager.currentPage + 1} / ${urls.size}", color = Color.White, fontSize = 15.sp, fontWeight = FontWeight.SemiBold,
                modifier = Modifier.align(Alignment.TopCenter).padding(top = 40.dp)
                    .clip(RoundedCornerShape(12.dp)).background(Color.Black.copy(alpha = 0.45f)).padding(horizontal = 12.dp, vertical = 4.dp),
            )
            Box(
                Modifier.align(Alignment.TopEnd).padding(top = 32.dp, end = 12.dp).size(44.dp).clip(RoundedCornerShape(22.dp))
                    .background(Color.Black.copy(alpha = 0.45f)).clickable(onClick = onClose),
                contentAlignment = Alignment.Center,
            ) { Text("✕", color = Color.White, fontSize = 18.sp) }
        }
    }
}

@Composable
private fun ZoomImage(url: String, onTap: () -> Unit) {
    var failed by remember(url) { mutableStateOf(false) }
    val img by produceState(Images.full.get(url), url) {
        if (value == null) { value = withContext(Dispatchers.IO) { Images.load(url, big = true) }; failed = value == null }
    }
    var scale by remember { mutableFloatStateOf(1f) }
    var offset by remember { mutableStateOf(Offset.Zero) }
    var box by remember { mutableStateOf(IntSize.Zero) }
    fun clamp(o: Offset): Offset {
        val mx = (scale - 1f) * box.width / 2f
        val my = (scale - 1f) * box.height / 2f
        return Offset(o.x.coerceIn(-mx, mx), o.y.coerceIn(-my, my))
    }
    Box(
        Modifier.fillMaxSize().onSizeChanged { box = it }
            .pointerInput(url) {
                detectTapGestures(
                    onTap = { onTap() },
                    onDoubleTap = { if (scale > 1f) { scale = 1f; offset = Offset.Zero } else scale = 2.5f },
                )
            }
            .pointerInput(url) {
                // 两指缩放；放大状态下单指拖动是平移。只有这两种情况才吃掉手势，平时单指左右滑交给翻页
                awaitEachGesture {
                    awaitFirstDown(requireUnconsumed = false)
                    do {
                        val ev = awaitPointerEvent()
                        val pinch = ev.changes.count { it.pressed } >= 2
                        if (pinch || scale > 1f) {
                            scale = (scale * ev.calculateZoom()).coerceIn(1f, 5f)
                            offset = if (scale <= 1f) Offset.Zero else clamp(offset + ev.calculatePan())
                            ev.changes.forEach { if (it.positionChanged()) it.consume() }
                        }
                    } while (ev.changes.any { it.pressed })
                }
            },
        contentAlignment = Alignment.Center,
    ) {
        val b = img
        when {
            b != null -> Image(
                b, null, contentScale = ContentScale.Fit,
                modifier = Modifier.fillMaxSize().graphicsLayer(scaleX = scale, scaleY = scale, translationX = offset.x, translationY = offset.y),
            )
            failed -> Text("图片加载失败", color = Color.White.copy(alpha = 0.7f), fontSize = 14.sp)
            else -> Text("加载中…", color = Color.White.copy(alpha = 0.7f), fontSize = 14.sp)
        }
    }
}
