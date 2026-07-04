import { useEffect, useMemo, useRef, useState } from "react";
import { invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";
import { getCurrentWindow } from "@tauri-apps/api/window";
import { open } from "@tauri-apps/plugin-dialog";
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

const formatSpeed = (bytesPerSecond: number) => {
  if (bytesPerSecond <= 0) return "0 B/s";
  return `${formatBytes(bytesPerSecond)}/s`;
};

const stageLabel = (stage: DownloadStage) => {
  switch (stage) {
    case "queued":
      return "排队";
    case "downloading":
      return "下载中";
    case "paused":
      return "已暂停";
    case "completed":
      return "完成";
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
  if (!raw) return "—";
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
      return course.workflowState === "completed" ? "已结束" : "—";
  }
};

const displayPath = (path: string) => {
  if (!path.trim()) return "未选择目录";
  if (path.length <= 52) return path;
  return `…${path.slice(-49)}`;
};

function App() {
  const [screen, setScreen] = useState<Screen>("welcome");
  const [auth, setAuth] = useState<AuthStatus>({ phase: "idle", message: "未登录" });
  const [courses, setCourses] = useState<Course[]>([]);
  const [course, setCourse] = useState<Course | null>(null);
  const [lessons, setLessons] = useState<Lesson[]>([]);
  const [selected, setSelected] = useState<Set<string>>(new Set());
  const [signals, setSignals] = useState<Set<string>>(new Set(["教师", "PPT"]));
  const [outputDir, setOutputDir] = useState("");
  const [concurrency, setConcurrency] = useState("16");
  const [courseQuery, setCourseQuery] = useState("");
  const [query, setQuery] = useState("");
  const [qrImage, setQrImage] = useState("");
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState("");
  const [tasks, setTasks] = useState<Record<string, DownloadProgress>>({});
  const [downloadSpeed, setDownloadSpeed] = useState(0);
  const speedSampleRef = useRef<{ taskBytes: Record<string, number>; at: number }>({
    taskBytes: {},
    at: Date.now(),
  });

  const refreshTasks = async () => {
    try {
      const result = await invoke<DownloadProgress[]>("list_download_tasks");
      setTasks(Object.fromEntries(result.map((task) => [task.taskId, task])));
    } catch {
      // ignore
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
    void invoke<number>("get_download_concurrency")
      .then((value) => setConcurrency(String(value)))
      .catch(() => undefined);
    void refreshTasks();
    return () => {
      stopAuth?.();
      stopProgress?.();
      stopQr?.();
    };
  }, []);

  useEffect(() => {
    const now = Date.now();
    const prev = speedSampleRef.current;
    let delta = 0;
    const nextBytes: Record<string, number> = {};

    for (const task of Object.values(tasks)) {
      if (task.stage !== "downloading") continue;
      const previous = prev.taskBytes[task.taskId];
      if (previous !== undefined && task.downloaded >= previous) {
        delta += task.downloaded - previous;
      }
      nextBytes[task.taskId] = task.downloaded;
    }

    const elapsed = (now - prev.at) / 1000;
    if (elapsed >= 0.2 && delta > 0) {
      setDownloadSpeed(delta / elapsed);
      speedSampleRef.current = { taskBytes: nextBytes, at: now };
    } else {
      speedSampleRef.current = { taskBytes: nextBytes, at: prev.at };
    }
  }, [tasks]);

  useEffect(() => {
    const timer = window.setInterval(() => {
      if (Date.now() - speedSampleRef.current.at > 2000) setDownloadSpeed(0);
    }, 500);
    return () => window.clearInterval(timer);
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
    setAuth({ phase: "qr-login", message: "等待扫码" });
    try {
      await invoke("open_login");
    } catch (reason) {
      setError(String(reason));
      setAuth({ phase: "error", message: "登录失败" });
    }
  };

  useEffect(() => {
    if (screen !== "welcome" || auth.phase !== "idle") return;
    void openLogin();
  }, [screen, auth.phase]);

  const refreshQrCode = async () => {
    setError("");
    try {
      await invoke("refresh_qr_code");
    } catch (reason) {
      setError(String(reason));
    }
  };

  const pickOutputDir = async () => {
    setError("");
    try {
      const selected = await open({
        directory: true,
        multiple: false,
        defaultPath: outputDir.trim() || undefined,
        title: "选择保存目录",
      });
      if (typeof selected === "string") setOutputDir(selected);
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
    return courses.filter((item) =>
      [item.name, item.courseCode, item.startAt ?? "", item.endAt ?? "", enrollmentLabel(item)]
        .join(" ")
        .toLowerCase()
        .includes(term),
    );
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

  const applyConcurrency = async (raw: string) => {
    const parsed = Number.parseInt(raw, 10);
    if (Number.isNaN(parsed)) return;
    try {
      const value = await invoke<number>("set_download_concurrency", { concurrency: parsed });
      setConcurrency(String(value));
    } catch (reason) {
      setError(String(reason));
    }
  };

  const startDownloads = async () => {
    const picked = lessons.filter((lesson) => selected.has(lesson.videoId));
    if (!picked.length || !signals.size || !course) return;
    setError("");
    try {
      await applyConcurrency(concurrency);
      await Promise.all(
        picked.map((lesson) => {
          const request: DownloadRequest = {
            lessonId: lesson.videoId,
            lessonTitle: lesson.title,
            beginTime: lesson.beginTime,
            signals: [...signals],
            outputDir: outputDir.trim() || null,
            courseName: course.name,
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
    try {
      await invoke("pause_download", { taskId });
    } catch (reason) {
      setError(String(reason));
    }
  };

  const resumeTask = async (taskId: string) => {
    try {
      await invoke("resume_download", { taskId });
    } catch (reason) {
      setError(String(reason));
    }
  };

  const cancelTask = async (taskId: string) => {
    try {
      await invoke("cancel_download", { taskId });
    } catch (reason) {
      setError(String(reason));
    }
  };

  const logout = async () => {
    await invoke("logout");
    setAuth({ phase: "idle", message: "未登录" });
    setQrImage("");
    setCourses([]);
    setLessons([]);
    setCourse(null);
    setTasks({});
    setScreen("welcome");
  };

  const closeApp = () => {
    void getCurrentWindow().close();
  };

  const activeTasks = Object.values(tasks).sort((a, b) => b.taskId.localeCompare(a.taskId));

  const downloadSummary = useMemo(() => {
    const list = activeTasks.filter((task) => task.stage !== "cancelled");
    let downloaded = 0;
    let total = 0;
    let knownTotal = true;
    let downloading = 0;
    let completed = 0;
    let queued = 0;

    for (const task of list) {
      downloaded += task.downloaded;
      if (task.total) {
        total += task.total;
      } else if (task.stage === "completed") {
        total += task.downloaded;
      } else {
        knownTotal = false;
      }
      if (task.stage === "downloading") downloading += 1;
      else if (task.stage === "completed") completed += 1;
      else if (task.stage === "queued") queued += 1;
    }

    const percent = knownTotal && total > 0 ? Math.min(100, (downloaded / total) * 100) : null;
    return { downloaded, total, percent, downloading, completed, queued, totalTasks: list.length, knownTotal };
  }, [activeTasks]);

  const pageTitle =
    screen === "welcome"
      ? "登录"
      : screen === "courses"
        ? "课程"
        : screen === "downloads"
          ? "下载"
          : shortCourseName(course?.name ?? "课堂录像");

  return (
    <main className="shell">
      <aside className="sidebar">
        <button type="button" className="window-close" onClick={closeApp} aria-label="关闭">
          <svg viewBox="0 0 24 24" width="14" height="14" aria-hidden>
            <path d="M6 6l12 12M18 6L6 18" fill="none" stroke="currentColor" strokeWidth="1.75" strokeLinecap="round" />
          </svg>
        </button>
        <div className="brand" data-tauri-drag-region>
          <div className="brand-icon" aria-hidden>✦</div>
          <div className="brand-text">Canvas 下载</div>
        </div>
        <nav className="sidebar-nav" aria-label="导航">
          <button type="button" className={screen === "welcome" ? "nav-link active" : "nav-link"} onClick={() => setScreen("welcome")}>
            登录
          </button>
          <button
            type="button"
            className={screen === "courses" || screen === "lessons" ? "nav-link active" : "nav-link"}
            disabled={!courses.length}
            onClick={() => setScreen("courses")}
          >
            课程
          </button>
          <button type="button" className={screen === "downloads" ? "nav-link active" : "nav-link"} onClick={() => setScreen("downloads")}>
            下载
          </button>
        </nav>
        <div className="sidebar-foot">{auth.message}</div>
      </aside>

      <div className="stage">
        <header className="page-header" data-tauri-drag-region>
          <div>
            {screen === "lessons" && course && (
              <div className="crumb">
                <button type="button" onClick={() => setScreen("courses")}>课程</button>
                <span>·</span>
                <span>{shortCourseName(course.name)}</span>
              </div>
            )}
            <h1 className="page-title">{pageTitle}</h1>
            {screen === "courses" && (
              <p className="page-sub">{visibleCourses.length} 门课程</p>
            )}
            {screen === "lessons" && (
              <p className="page-sub">已选 {selected.size} 讲</p>
            )}
            {screen === "downloads" && downloadSummary.totalTasks > 0 && (
              <p className="page-sub">
                {downloadSummary.completed}/{downloadSummary.totalTasks} 已完成
                {downloadSummary.downloading > 0 && ` · ${downloadSummary.downloading} 个下载中`}
              </p>
            )}
          </div>
          <div className="header-actions">
            {screen === "lessons" && (
              <button type="button" className="btn" onClick={() => setScreen("courses")}>返回课程</button>
            )}
            {auth.phase === "authorized" && (
              <button type="button" className="btn-ghost" onClick={logout}>退出</button>
            )}
            {screen === "courses" && auth.phase === "authorized" && (
              <button type="button" className="btn" onClick={refreshCourses} disabled={busy}>刷新</button>
            )}
            {screen === "downloads" && (
              <button type="button" className="btn" onClick={() => void refreshTasks()}>刷新</button>
            )}
          </div>
        </header>

        {error && <div className="alert page-body">{error}</div>}

        <div className={screen === "lessons" ? "page-body page-body-fill" : "page-body"}>
          {screen === "welcome" && (
            <section className="login-screen">
              <div className="card login-card">
                <div className="login-grid">
                  <div className="login-copy">
                    <div className="brand-icon brand-icon-lg" aria-hidden>✦</div>
                    <h2 className="login-title">Canvas 下载</h2>
                    <p className="login-lead">上海交通大学 · 交我办扫码登录</p>
                    <ol className="login-steps">
                      <li>扫码登录 Canvas</li>
                      <li>选择课程与讲次</li>
                      <li>下载课堂录像</li>
                    </ol>
                  </div>
                  <div className="login-qr">
                    <div className="qr-frame">
                      {qrImage ? (
                        <img src={`data:image/png;base64,${qrImage}`} alt="扫码登录" onClick={refreshQrCode} />
                      ) : (
                        <div className="qr-empty">
                          <div className="spinner" />
                          <span>{auth.phase === "error" ? "加载失败" : "正在获取二维码"}</span>
                        </div>
                      )}
                    </div>
                    <p className="login-status">{auth.message}</p>
                    <div className="login-actions">
                      <button type="button" className="btn btn-accent" onClick={openLogin}>
                        {auth.phase === "qr-login" ? "刷新二维码" : "重新登录"}
                      </button>
                      {auth.phase === "authorized" && (
                        <button type="button" className="btn" onClick={refreshCourses}>进入课程</button>
                      )}
                    </div>
                  </div>
                </div>
              </div>
            </section>
          )}

          {screen === "courses" && (
            <section className="page-scroll">
              <div className="card">
                <div className="card-body">
                  <div className="toolbar">
                    <input
                      className="input input-search"
                      value={courseQuery}
                      onChange={(event) => setCourseQuery(event.target.value)}
                      placeholder="搜索课程"
                    />
                  </div>
                  {visibleCourses.length ? (
                    <div className="list">
                      {visibleCourses.map((item) => (
                        <button type="button" className="list-item" key={item.id} onClick={() => chooseCourse(item)}>
                          <div className="list-item-main">
                            <strong>{shortCourseName(item.name)}</strong>
                            <span>{item.courseCode || "—"} · {formatCourseDate(item)}</span>
                          </div>
                          <span className={item.enrollmentState === "active" ? "chip live" : "chip"}>
                            {enrollmentLabel(item)}
                          </span>
                        </button>
                      ))}
                    </div>
                  ) : (
                    <div className="empty">{courses.length ? "无匹配课程" : "暂无课程"}</div>
                  )}
                </div>
              </div>
            </section>
          )}

          {screen === "lessons" && (
            <section className="lesson-grid">
              <div className="lesson-scroll">
                <div className="card">
                  <div className="card-header">
                    <h2>讲次</h2>
                    <div className="header-actions-inline">
                      <button
                        type="button"
                        className="btn"
                        onClick={() => setSelected(new Set(visibleLessons.map((lesson) => lesson.videoId)))}
                      >
                        全选
                      </button>
                      <button type="button" className="btn" onClick={() => setSelected(new Set())}>清空</button>
                    </div>
                  </div>
                  <div className="card-body">
                    <div className="toolbar">
                      <input
                        className="input input-search"
                        value={query}
                        onChange={(event) => setQuery(event.target.value)}
                        placeholder="搜索讲次"
                      />
                    </div>
                    {visibleLessons.length ? (
                      <div className="list">
                        {visibleLessons.map((lesson) => (
                          <div
                            key={lesson.videoId}
                            className={selected.has(lesson.videoId) ? "list-item selected" : "list-item"}
                            onClick={() => toggleLesson(lesson.videoId)}
                            onKeyDown={(event) => {
                              if (event.key === "Enter" || event.key === " ") toggleLesson(lesson.videoId);
                            }}
                            role="button"
                            tabIndex={0}
                          >
                            <div className="list-item-check">
                              <input
                                type="checkbox"
                                checked={selected.has(lesson.videoId)}
                                onChange={() => toggleLesson(lesson.videoId)}
                                onClick={(event) => event.stopPropagation()}
                              />
                              <div className="list-item-main">
                                <strong>{lesson.title}</strong>
                                <span>{lesson.beginTime} · {lesson.classroom || "—"}</span>
                              </div>
                            </div>
                          </div>
                        ))}
                      </div>
                    ) : (
                      <div className="empty">暂无开放讲次</div>
                    )}
                  </div>
                </div>
              </div>

              <aside className="card side-card">
                <div className="card-body">
                  <div className="field">
                    <span>分轨</span>
                    <div className="checks">
                      {["教师", "PPT"].map((signal) => (
                        <label key={signal}>
                          <input
                            type="checkbox"
                            checked={signals.has(signal)}
                            onChange={() => toggleSignal(signal)}
                          />
                          {signal}
                        </label>
                      ))}
                    </div>
                  </div>
                  <div className="field">
                    <span>保存目录</span>
                    <div className="path-picker">
                      <div className="path-value" title={outputDir}>{displayPath(outputDir)}</div>
                      <button type="button" className="btn" onClick={() => void pickOutputDir()}>选择</button>
                    </div>
                  </div>
                  <label className="field">
                    <span>并发</span>
                    <input
                      className="input"
                      type="number"
                      min={1}
                      max={64}
                      value={concurrency}
                      onChange={(event) => setConcurrency(event.target.value)}
                      onBlur={() => void applyConcurrency(concurrency)}
                    />
                  </label>
                  <button
                    type="button"
                    className="btn btn-accent"
                    disabled={!selected.size || !signals.size}
                    onClick={startDownloads}
                  >
                    下载
                  </button>
                </div>
              </aside>
            </section>
          )}

          {screen === "downloads" && (
            <section className="page-scroll">
              {activeTasks.length ? (
                <>
                  <div className="download-summary card">
                    <div className="download-summary-top">
                      <div>
                        <span className="download-summary-label">总进度</span>
                        <strong className="download-summary-percent">
                          {downloadSummary.percent !== null
                            ? `${Math.round(downloadSummary.percent)}%`
                            : "—"}
                        </strong>
                      </div>
                      <div className="download-summary-speed">{formatSpeed(downloadSpeed)}</div>
                    </div>
                    <div className="progress progress-lg">
                      <i
                        style={{
                          width: `${downloadSummary.percent ?? (downloadSummary.downloaded > 0 ? 8 : 0)}%`,
                        }}
                      />
                    </div>
                    <div className="download-summary-meta">
                      <span>
                        {formatBytes(downloadSummary.downloaded)}
                        {downloadSummary.knownTotal && downloadSummary.total > 0
                          ? ` / ${formatBytes(downloadSummary.total)}`
                          : ""}
                      </span>
                      <span>
                        {downloadSummary.downloading > 0
                          ? `${downloadSummary.downloading} 个进行中`
                          : downloadSummary.queued > 0
                            ? `${downloadSummary.queued} 个排队中`
                            : "全部完成"}
                      </span>
                    </div>
                  </div>
                  <div className="task-list">
                  {activeTasks.map((task) => {
                    const percent = task.total
                      ? Math.min(100, (task.downloaded / task.total) * 100)
                      : task.stage === "completed"
                        ? 100
                        : 8;
                    const canPause = task.stage === "downloading" || task.stage === "queued";
                    const canResume = task.stage === "paused" || task.stage === "failed";
                    const canCancel = task.stage !== "completed" && task.stage !== "cancelled";
                    const title = task.fileName || `${task.lessonTitle} · ${task.signal}`;
                    return (
                      <article className="task-card" key={task.taskId}>
                        <div className="task-top">
                          <div>
                            <strong>{title}</strong>
                            <span>{task.lessonTitle} · {task.signal}</span>
                          </div>
                          <span className={`status ${task.stage}`}>{stageLabel(task.stage)}</span>
                        </div>
                        <div className="task-meta">
                          <span>
                            {task.stage === "failed" || task.stage === "cancelled"
                              ? task.message
                              : `${formatBytes(task.downloaded)}${task.total ? ` / ${formatBytes(task.total)}` : ""}`}
                          </span>
                          <span>{Math.round(percent)}%</span>
                        </div>
                        <div className="progress"><i style={{ width: `${percent}%` }} /></div>
                        <div className="task-actions">
                          {canPause && <button type="button" className="btn" onClick={() => void pauseTask(task.taskId)}>暂停</button>}
                          {canResume && <button type="button" className="btn btn-accent" onClick={() => void resumeTask(task.taskId)}>继续</button>}
                          {canCancel && <button type="button" className="btn-ghost" onClick={() => void cancelTask(task.taskId)}>取消</button>}
                        </div>
                      </article>
                    );
                  })}
                  </div>
                </>
              ) : (
                <div className="card"><div className="empty">暂无任务</div></div>
              )}
            </section>
          )}
        </div>
      </div>

      {busy && (
        <div className="busy">
          <div className="spinner" />
        </div>
      )}
    </main>
  );
}

export default App;
