/** A file or directory entry from fs9 readdir API */
export interface Fs9FileInfo {
  path: string;
  type: 'file' | 'dir';
  size: number;
  mode: number;
  mtime: string;
}

/** Response from fs9 stat API */
export interface Fs9StatResponse {
  path: string;
  is_dir: boolean;
  is_file: boolean;
  size: number;
  mode: number;
  mtime: number;
}

/** Options for fs9 list operation */
export interface Fs9ListOptions {
  recursive?: boolean;
}
