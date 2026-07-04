import { useEffect, useMemo, useState } from "react";
import { invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";
import type {
  AuthStatus,
  Course,
  DownloadProgress,
  DownloadRequest,
  Lesson,
} from "./types";

type Screen = "welcome" | "courses" | "lessons";

const formatBytes = (value: number) => {
  if (!value) return "0 B";
  const units = ["B", "KB", "MB", "GB"];
  const index = Math.min(Math.floor(Math.log(value) / Math.log(1024)), units.length - 1);
  return `${(value / 1024 ** index).toFixed(index > 1 ? 1 : 0)} ${units[index]}`;
};

const shortCourseName = (name: string) =>
  name.replace(/^本-\([^)]*\)-[^-]+-\d+-/, "").trim() || name;

function App() {
  const [screen, setScreen] = useState<Screen>("welcome");
  const [auth, setAuth] = useState<AuthStatus>({ phase: "idle", message: "尚未连接 Canvas" });
  const [courses, setCourses] = useState<Course[]>([]);
  const [course, setCourse] = useState<Course | null>(null);
  const [lessons, setLessons] = useState<Lesson[]>([]);
  const [selected, setSelected] = useState<Set<string>>(new Set());
  const [signals, setSignals] = useState<Set<string>>(new Set(["教师", "PPT"]));
  const [includeSubtitles, setIncludeSubtitles] = useState(true);
  const [outputDir, setOutputDir] = useState("");
  const [query, setQuery] = useState("");
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState("");
  const [tasks, setTasks] = useState<Record<string, DownloadProgress>>({});

  useEffect(() => {
    let stopAuth: (() => void) | undefined;
    let stopProgress: (() => void) | undefined;
    void listen<AuthStatus>("auth-status", (event) => {
      setAuth(event.payload);
      if (event.payload.phase === "authorized") void refreshCourses();
    }).then((unlisten) => (stopAuth = unlisten));
    void listen<DownloadProgress>("download-progress", (event) => {
      const update = event.payload;
      setTasks((current) => ({ ...current, [update.taskId]: update }));
    }).then((unlisten) => (stopProgress = unlisten));
    void invoke<string>("default_download_dir").then(setOutputDir).catch(() => undefined);
    return () => {
      stopAuth?.();
      stopProgress?.();
    };
  }, []);

  const refreshCourses = async () => {
    setBusy(true);
    setError("");
    try {
      const result = await invoke<Course[]>("list_courses");
      setCourses(result);
      setScreen("courses");
    } catch (reason) {
      setError(String(reason));
    } finally {
      setBusy(false);
    }
  };

  const openLogin = async () => {
    setError("");
    setAuth({ phase: "canvas-login", message: "正在打开扫码登录窗口" });
    try {
      await invoke("open_login");
    } catch (reason) {
      setError(String(reason));
      setAuth({ phase: "error", message: "登录窗口打开失败" });
    }
  };

  const chooseCourse = async (nextCourse: Course) => {
    setBusy(true);
    setError("");
    setCourse(nextCourse);
    try {
      const result = await invoke<Lesson[]>("load_lessons", { courseId: nextCourse.id });
      setLessons(result);
      setSelected(new Set());
      setScreen("lessons");
    } catch (reason) {
      setError(String(reason));
    } finally {
      setBusy(false);
    }
  };

  const visibleLessons = useMemo(() => {
    const term = query.trim().toLowerCase();
    return lessons.filter(
      (lesson) =>
        lesson.auditStatus === 3 &&
        (!term || `${lesson.title} ${lesson.beginTime} ${lesson.classroom}`.toLowerCase().includes(term)),
    );
  }, [lessons, query]);

  const toggleLesson = (videoId: string) => {
    setSelected((current) => {
      const next = new Set(current);
      if (next.has(videoId)) next.delete(videoId);
      else next.add(videoId);
      return next;
    });
  };

  const toggleSignal = (signal: string) => {
    setSignals((current) => {
      const next = new Set(current);
      if (next.has(signal)) next.delete(signal);
      else next.add(signal);
      return next;
    });
  };

  const startDownloads = async () => {
    const picked = lessons.filter((lesson) => selected.has(lesson.videoId));
    if (!picked.length || (!signals.size && !includeSubtitles)) return;
    setError("");
    try {
      await Promise.all(
        picked.map((lesson) => {
          const request: DownloadRequest = {
            lessonId: lesson.videoId,
            lessonTitle: lesson.title,
            beginTime: lesson.beginTime,
            signals: [...signals],
            includeSubtitles,
            outputDir: outputDir.trim() || null,
          };
          return invoke<string>("start_download", { request });
        }),
      );
      setSelected(new Set());
    } catch (reason) {
      setError(String(reason));
    }
  };

  const logout = async () => {
    await invoke("logout");
    setAuth({ phase: "idle", message: "尚未连接 Canvas" });
    setCourses([]);
    setLessons([]);
    setCourse(null);
    setScreen("welcome");
  };

  const activeTasks = Object.values(tasks).sort((a, b) => b.taskId.localeCompare(a.taskId));
  const statusTone = auth.phase === "authorized" ? "ok" : auth.phase === "error" ? "bad" : "idle";

  return (
    <main className="app-shell">
      <aside className="sidebar">
        <div className="brand">
          <div className="brand-mark">C</div>
          <div>
            <strong>Canvas Pocket</strong>
            <span>SJTU 课程归档</span>
          </div>
        </div>

        <nav className="steps" aria-label="操作步骤">
          <button className={screen === "welcome" ? "step active" : "step"} onClick={() => setScreen("welcome")}>
            <span>01</span>扫码登录
          </button>
          <button className={screen === "courses" ? "step active" : "step"} disabled={!courses.length} onClick={() => setScreen("courses")}>
            <span>02</span>选择课程
          </button>
          <button className={screen === "lessons" ? "step active" : "step"} disabled={!course} onClick={() => setScreen("lessons")}>
            <span>03</span>下载录像
          </button>
        </nav>

        <div className="sidebar-foot">
          <div className={`status-dot ${statusTone}`} />
          <div>
            <small>连接状态</small>
            <span>{auth.message}</span>
          </div>
        </div>
      </aside>

      <section className="workspace">
        <header className="topbar">
          <div>
            <p className="eyebrow">CANVAS @ SJTU</p>
            <h1>{screen === "welcome" ? "把课程带走，慢慢消化。" : screen === "courses" ? "选择一门课程" : shortCourseName(course?.name ?? "课程录像")}</h1>
          </div>
          {auth.phase === "authorized" && (
            <button className="ghost-button" onClick={logout}>退出登录</button>
          )}
        </header>

        {error && <div className="error-banner"><strong>没有走通</strong><span>{error}</span></div>}

        {screen === "welcome" && (
          <section className="welcome-grid">
            <div className="hero-card">
              <div className="hero-number">01</div>
              <p>只使用交我办扫码</p>
              <h2>不输入密码，<br />也不保存密码。</h2>
              <p className="muted">登录窗口会自动进入 jAccount，并隐藏账号密码与短信入口。扫码完成后，课程会在这里出现。</p>
              <div className="hero-actions">
                <button className="primary-button" onClick={openLogin}>打开扫码登录</button>
                {auth.phase === "authorized" && <button className="text-button" onClick={refreshCourses}>刷新课程</button>}
              </div>
            </div>
            <div className="promise-stack">
              <article><span>双路</span><h3>教师画面 + PPT</h3><p>保留原始 1080p 分轨，也为后续合成留足余地。</p></article>
              <article><span>字幕</span><h3>AI 字幕另存 SRT</h3><p>保留毫秒时间轴，中文、英文翻译有就一起带走。</p></article>
              <article><span>本地</span><h3>令牌只活在内存</h3><p>课程资料落在你选择的目录，登录凭据不写入项目。</p></article>
            </div>
          </section>
        )}

        {screen === "courses" && (
          <section>
            <div className="section-tools">
              <p>{courses.length} 门活动课程</p>
              <button className="ghost-button" onClick={refreshCourses} disabled={busy}>重新读取</button>
            </div>
            <div className="course-grid">
              {courses.map((item, index) => (
                <button className="course-card" key={item.id} onClick={() => chooseCourse(item)} disabled={busy}>
                  <span>{String(index + 1).padStart(2, "0")}</span>
                  <h2>{shortCourseName(item.name)}</h2>
                  <p>{item.courseCode || `课程 ${item.id}`}</p>
                  <div>查看课堂录像 →</div>
                </button>
              ))}
            </div>
          </section>
        )}

        {screen === "lessons" && (
          <section className="lesson-layout">
            <div className="lesson-main">
              <div className="lesson-toolbar">
                <input value={query} onChange={(event) => setQuery(event.target.value)} placeholder="搜索讲次、日期或教室" />
                <button className="ghost-button" onClick={() => setSelected(new Set(visibleLessons.map((lesson) => lesson.videoId)))}>全选开放讲次</button>
                <button className="text-button" onClick={() => setSelected(new Set())}>清空</button>
              </div>
              <div className="lesson-list">
                {visibleLessons.map((lesson) => (
                  <label className={selected.has(lesson.videoId) ? "lesson-row selected" : "lesson-row"} key={lesson.videoId}>
                    <input type="checkbox" checked={selected.has(lesson.videoId)} onChange={() => toggleLesson(lesson.videoId)} />
                    <span className="custom-check" />
                    <div className="lesson-index">{lesson.title.replace(/[^0-9]/g, "").padStart(2, "0")}</div>
                    <div className="lesson-copy"><strong>{lesson.title}</strong><span>{lesson.beginTime} · {lesson.classroom || "教室未标注"}</span></div>
                    <div className="lesson-badge">开放</div>
                  </label>
                ))}
              </div>
            </div>

            <aside className="download-panel">
              <p className="eyebrow">DOWNLOAD SET</p>
              <h2>{selected.size} 讲待下载</h2>
              <div className="option-group">
                <span>视频分轨</span>
                {["教师", "PPT"].map((signal) => (
                  <label key={signal}><input type="checkbox" checked={signals.has(signal)} onChange={() => toggleSignal(signal)} /><i />{signal}</label>
                ))}
              </div>
              <div className="option-group">
                <span>辅助文件</span>
                <label><input type="checkbox" checked={includeSubtitles} onChange={(event) => setIncludeSubtitles(event.target.checked)} /><i />AI 字幕 .srt</label>
              </div>
              <label className="path-field"><span>保存目录</span><input value={outputDir} onChange={(event) => setOutputDir(event.target.value)} /></label>
              <button className="primary-button wide" disabled={!selected.size || (!signals.size && !includeSubtitles)} onClick={startDownloads}>开始下载</button>
              <small>并发数固定为 2，避免挤占校园网和视频服务。</small>
            </aside>
          </section>
        )}

        {busy && <div className="busy-layer"><div className="spinner" /><span>正在读取 Canvas…</span></div>}

        {activeTasks.length > 0 && (
          <section className="task-drawer">
            <header><strong>下载任务</strong><span>{activeTasks.filter((task) => task.stage === "completed").length}/{activeTasks.length} 完成</span></header>
            {activeTasks.slice(0, 6).map((task) => {
              const percent = task.total ? Math.min(100, (task.downloaded / task.total) * 100) : task.stage === "completed" ? 100 : 8;
              return <div className="task-row" key={task.taskId}>
                <div><strong>{task.fileName || task.message}</strong><span>{task.stage === "failed" ? task.message : `${formatBytes(task.downloaded)}${task.total ? ` / ${formatBytes(task.total)}` : ""}`}</span></div>
                <div className="progress-track"><i style={{ width: `${percent}%` }} /></div>
              </div>;
            })}
          </section>
        )}
      </section>
    </main>
  );
}

export default App;

