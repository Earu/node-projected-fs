/* tslint:disable */
/* eslint-disable */

/* auto-generated by NAPI-RS */

export interface FileSystemEvent {
  eventType: string
  path: string
  objectType: string
}
export type JsFuseFS = FuseFS
export declare class FuseFS {
  constructor()
  mount(path: string, totalSpaceBytes: number): Promise<void>
  unmount(): Promise<void>
  addFile(path: string, content: Buffer): Promise<void>
  addDirectory(path: string): Promise<void>
  removePath(path: string): Promise<void>
  on(callback: (...args: any[]) => any): void
}
