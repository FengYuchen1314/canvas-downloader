export type AuthPhase = "idle" | "canvas-login" | "qr-login" | "authorized" | "error";

export interface AuthStatus {
  phase: AuthPhase;
  message: string;
}

export interface Course {
  id: number;
  name: string;
  courseCode: string;
  startAt?: string | null;
  endAt?: string | null;
  createdAt?: string | null;
  workflowState?: string | null;
  enrollmentState?: string | null;
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

export type DownloadStage =
  | "queued"
  | "downloading"
  | "paused"
  | "completed"
  | "failed"
  | "cancelled";

export interface DownloadProgress {
  taskId: string;
  lessonId: string;
  lessonTitle: string;
  signal: string;
  fileName: string;
  stage: DownloadStage;
  downloaded: number;
  total: number | null;
  message: string;
}
