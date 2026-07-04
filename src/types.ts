export type AuthPhase = "idle" | "canvas-login" | "qr-login" | "authorized" | "error";

export interface AuthStatus {
  phase: AuthPhase;
  message: string;
}

export interface Course {
  id: number;
  name: string;
  courseCode: string;
}

export interface Lesson {
  videoId: string;
  title: string;
  beginTime: string;
  endTime: string;
  classroom: string;
  auditStatus: number;
}

export interface DownloadRequest {
  lessonId: string;
  lessonTitle: string;
  beginTime: string;
  signals: string[];
  outputDir: string | null;
}

export interface DownloadProgress {
  taskId: string;
  lessonId: string;
  fileName: string;
  stage: "queued" | "downloading" | "completed" | "failed";
  downloaded: number;
  total: number | null;
  message: string;
}

