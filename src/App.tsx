import { useEffect, useMemo, useState } from "react";
import { invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";
import type {
  AuthStatus,
  Course,
  DownloadProgress,
  DownloadRequest,
  DownloadStage,
  Lesson,
} from "./types";

type Screen = "welcome" | "courses" | "lessons" | "downloads";

const formatBytes = (value: number) => {
  if (!value) return "0 B";
  const units = ["B", "KB", "MB", "GB"];
  const index = Math.min(Math.floor(Math.log(value) / Math.log(1024)), units.length - 1);
  return `${(value / 1024 ** index).toFixed(index > 1 ? 1 : 0)} ${units[index]}`;
};

const stageLabel = (stage: DownloadStage) => {
  switch (stage) {
    case "queued":
      return "排队中";
    case "downloading":
      return "下载中";
    case "paused":
      return "已暂停";
    case "completed":
      return "已完成";
    case "failed":
      return "失败";
    case "cancelled":
      return "已取消";
  }
};

const shortCourseName = (name: string) =>
  name.replace(/^本-\([^)]*\)-[^-]+-\d+-/, "").trim() || name;

const formatCourseDate = (course: Course) => {
  const raw = course.endAt || course.startAt || course.createdAt;
  if (!raw) return "日期未标注";
  const parsed = Date.parse(raw);
  if (Number.isNaN(parsed)) return raw.slice(0, 10);
  return new Intl.DateTimeFormat("zh-CN", {
    year: "numeric",
    month: "2-digit",
    day: "2-digit",
  }).format(parsed);
};

const enrollmentLabel = (course: Course) => {
  switch (course.enrollmentState) {
    case "active":
      return "进行中";
    case "completed":
      return "已结束";
    case "invited_or_pending":
      return "待加入";
    default:
      return course.workflowState === "completed" ? "已结束" : "历史课程";
  }
};

function App() {
  const [screen, setScreen] = useState<Screen>("welcome");
  const [auth, setAuth] = useState<AuthStatus>({ phase: "idle", message: "尚未连接 Canvas" });
  const [courses, setCourses] = useState<Course[]>([]);
  const [course, setCourse] = useState<Course | null>(null);
  const [lessons, setLessons] = useState<Lesson[]>([]);
  const [selected, setSelected] = useState<Set<string>>(new Set());
  const [signals, setSignals] = useState<Set<string>>(new Set(["教师", "PPT"]));
  const [outputDir, setOutputDir] = useState("");
  const [courseQuery, setCourseQuery] = useState("");
  const [query, setQuery] = useState("");
  const [qrImage, setQrImage] = useState("");
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState("");
  const [tasks, setTasks] = useState<Record<string, DownloadProgress>>({});

  const refreshTasks = async () => {
    try {
      const result = await invoke<DownloadProgress[]>("list_download_tasks");
      setTasks(Object.fromEntries(result.map((task) => [task.taskId, task])));
    } catch {
      // ignore when backend is not ready yet
    }
  };

  useEffect(() => {
    let stopAuth: (() => void) | undefined;
    let stopProgress: (() => void) | undefined;
    let stopQr: (() => void) | undefined;
    void listen<AuthStatus>("auth-status", (event) => {
      setAuth(event.payload);
      if (event.payload.phase === "authorized") void refreshCourses();
    }).then((unlisten) => (stopAuth = unlisten));
    void listen<string>("qr-code-update", (event) => {
      setQrImage(event.payload);
    }).then((unlisten) => (stopQr = unlisten));
    void listen<DownloadProgress>("download-progress", (event) => {
      const update = event.payload;
      setTasks((current) => ({ ...current, [update.taskId]: update }));
    }).then((unlisten) => (stopProgress = unlisten));
    void invoke<string>("default_download_dir").then(setOutputDir).catch(() => undefined);
    void refreshTasks();
    return () => {
      stopAuth?.();
      stopProgress?.();
      stopQr?.();
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
    setQrImage("");
    setAuth({ phase: "qr-login", message: "正在获取二维码…" });
    try {
      await invoke("open_login");
    } catch (reason) {
      setError(String(reason));
      setAuth({ phase: "error", message: "登录启动失败" });
    }
  };

  const refreshQrCode = async () => {
    setError("");
    try {
      await invoke("refresh_qr_code");
    } catch (reason) {
      setError(String(reason));
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

  const visibleCourses = useMemo(() => {
    const term = courseQuery.trim().toLowerCase();
    if (!term) return courses;
    return courses.filter((item) => {
      const haystack = [
        item.name,
        item.courseCode,
        item.startAt ?? "",
        item.endAt ?? "",
        enrollmentLabel(item),
      ]
        .join(" ")
        .toLowerCase();
      return haystack.includes(term);
    });
  }, [courses, courseQuery]);

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
    if (!picked.length || !signals.size) return;
    setError("");
    try {
      await Promise.all(
        picked.map((lesson) => {
          const request: DownloadRequest = {
            lessonId: lesson.videoId,
            lessonTitle: lesson.title,
            beginTime: lesson.beginTime,
            signals: [...signals],
            outputDir: outputDir.trim() || null,
          };
          return invoke<string[]>("start_download", { request });
        }),
      );
      setSelected(new Set());
      setScreen("downloads");
      await refreshTasks();
    } catch (reason) {
      setError(String(reason));
    }
  };

  const pauseTask = async (taskId: string) => {
    setError("");
    try {
      await invoke("pause_download", { taskId });
    } catch (reason) {
      setError(String(reason));
    }
  };

  const resumeTask = async (taskId: string) => {
    setError("");
    try {
      await invoke("resume_download", { taskId });
    } catch (reason) {
      setError(String(reason));
    }
  };

  const cancelTask = async (taskId: string) => {
    setError("");
    try {
      await invoke("cancel_download", { taskId });
    } catch (reason) {
      setError(String(reason));
    }
  };

  const logout = async () => {
    await invoke("logout");
    setAuth({ phase: "idle", message: "尚未连接 Canvas" });
    setQrImage("");
    setCourses([]);
    setLessons([]);
    setCourse(null);
    setTasks({});
    setScreen("welcome");
  };

  const activeTasks = Object.values(tasks).sort((a, b) => b.taskId.localeCompare(a.taskId));
  const activeCount = activeTasks.filter((task) => task.stage === "downloading" || task.stage === "queued").length;
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
          <button className={screen === "downloads" ? "step active" : "step"} onClick={() => setScreen("downloads")}>
            <span>04</span>下载任务{activeCount > 0 ? ` (${activeCount})` : ""}
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
            <h1>
              {screen === "welcome"
                ? "把课程带走，慢慢消化。"
                : screen === "courses"
                  ? "选择一门课程"
                  : screen === "downloads"
                    ? "下载任务"
                    : shortCourseName(course?.name ?? "课程录像")}
            </h1>
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
              <p className="muted">登录流程与 sjtu-canvas-video-download 一致：先进入 jAccount，再用交我办扫码。扫码完成后，课程会在这里出现。</p>
              <div className="qr-panel">
                {qrImage ? (
                  <img className="qr-image" src={`data:image/png;base64,${qrImage}`} alt="交我办扫码登录" onClick={refreshQrCode} />
                ) : (
                  <div className="qr-placeholder">{auth.phase === "qr-login" ? "正在加载二维码…" : "点击下方按钮开始扫码"}</div>
                )}
                {qrImage && <small>点击二维码可刷新</small>}
              </div>
              <div className="hero-actions">
                <button className="primary-button" onClick={openLogin}>
                  {auth.phase === "qr-login" ? "重新获取二维码" : "开始扫码登录"}
                </button>
                {qrImage && <button className="text-button" onClick={refreshQrCode}>刷新二维码</button>}
                {auth.phase === "authorized" && <button className="text-button" onClick={refreshCourses}>刷新课程</button>}
              </div>
            </div>
            <div className="promise-stack">
              <article><span>直链</span><h3>rtmpUrlHdv 下载</h3><p>与 sjtu-canvas-video-download 相同，直接拉取平台返回的 MP4 地址。</p></article>
              <article><span>本地</span><h3>令牌只活在内存</h3><p>课程资料落在你选择的目录，登录凭据不写入项目。</p></article>
            </div>
          </section>
        )}

        {screen === "courses" && (
          <section>
            <div className="section-tools course-tools">
              <input
                className="search-input"
                value={courseQuery}
                onChange={(event) => setCourseQuery(event.target.value)}
                placeholder="搜索课程名、代码或日期"
              />
              <p>{visibleCourses.length} / {courses.length} 门课程</p>
              <button className="ghost-button" onClick={refreshCourses} disabled={busy}>重新读取</button>
            </div>
            <div className="course-grid">
              {visibleCourses.map((item, index) => (
                <button className="course-card" key={item.id} onClick={() => chooseCourse(item)} disabled={busy}>
                  <span>{String(index + 1).padStart(2, "0")}</span>
                  <h2>{shortCourseName(item.name)}</h2>
                  <p>{item.courseCode || `课程 ${item.id}`} · {formatCourseDate(item)}</p>
                  <div className="course-card-foot">
                    <em>{enrollmentLabel(item)}</em>
                    <span>查看课堂录像 →</span>
                  </div>
                </button>
              ))}
            </div>
            {!visibleCourses.length && (
              <div className="downloads-empty">
                <p>{courses.length ? "没有匹配的课程。" : "还没有读取到课程。"}</p>
              </div>
            )}
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
              <label className="path-field"><span>保存目录</span><input value={outputDir} onChange={(event) => setOutputDir(event.target.value)} /></label>
              <button className="primary-button wide" disabled={!selected.size || !signals.size} onClick={startDownloads}>开始下载</button>
              <small>每个分轨单独成任务，支持暂停与断点续传。并发数固定为 2。</small>
            </aside>
          </section>
        )}

        {screen === "downloads" && (
          <section className="downloads-page">
            <div className="section-tools">
              <p>{activeTasks.length} 个任务 · {activeTasks.filter((task) => task.stage === "completed").length} 已完成</p>
              <button className="ghost-button" onClick={() => void refreshTasks()}>刷新状态</button>
            </div>
            {activeTasks.length === 0 ? (
              <div className="downloads-empty">
                <p>还没有下载任务。</p>
                <button className="ghost-button" disabled={!course} onClick={() => setScreen("lessons")}>去选择讲次</button>
              </div>
            ) : (
              <div className="downloads-list">
                {activeTasks.map((task) => {
                  const percent = task.total
                    ? Math.min(100, (task.downloaded / task.total) * 100)
                    : task.stage === "completed"
                      ? 100
                      : task.downloaded > 0
                        ? Math.max(8, Math.min(96, (task.downloaded % (50 * 1024 * 1024)) / (50 * 1024 * 1024) * 100))
                        : 8;
                  const canPause = task.stage === "downloading" || task.stage === "queued";
                  const canResume = task.stage === "paused" || task.stage === "failed";
                  const canCancel = task.stage !== "completed" && task.stage !== "cancelled";
                  const title = task.fileName || `${task.lessonTitle} · ${task.signal}`;
                  return (
                    <article className={`download-task ${task.stage}`} key={task.taskId}>
                      <div className="download-task-head">
                        <div>
                          <strong>{title}</strong>
                          <span>{task.lessonTitle} · {task.signal}</span>
                        </div>
                        <em>{stageLabel(task.stage)}</em>
                      </div>
                      <div className="download-task-meta">
                        <span>
                          {task.stage === "failed" || task.stage === "cancelled"
                            ? task.message
                            : `${formatBytes(task.downloaded)}${task.total ? ` / ${formatBytes(task.total)}` : ""}`}
                        </span>
                        <span>{Math.round(percent)}%</span>
                      </div>
                      <div className="progress-track wide-track"><i style={{ width: `${percent}%` }} /></div>
                      <div className="download-task-actions">
                        {canPause && <button className="ghost-button" onClick={() => void pauseTask(task.taskId)}>暂停</button>}
                        {canResume && <button className="primary-button compact" onClick={() => void resumeTask(task.taskId)}>继续</button>}
                        {canCancel && <button className="text-button" onClick={() => void cancelTask(task.taskId)}>取消</button>}
                      </div>
                    </article>
                  );
                })}
              </div>
            )}
          </section>
        )}

        {busy && <div className="busy-layer"><div className="spinner" /><span>正在读取 Canvas…</span></div>}
      </section>
    </main>
  );
}

export default App;
