/** A file or directory entry from fs9 readdir/stat API */
export interface Fs9FileEntry {
  path: string;
  size: number;
  file_type: 'regular' | 'directory' | 'symlink';
  mode: number;
  uid: number;
  gid: number;
  atime: number;
  mtime: number;
  ctime: number;
  etag: string;
  symlink_target?: string;
}

/** Options for fs9 list operation */
export interface Fs9ListOptions {
  recursive?: boolean;
}
