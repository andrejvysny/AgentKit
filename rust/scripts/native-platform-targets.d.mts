export interface NativePlatformTarget {
  readonly triple: string;
  readonly package: string;
  readonly key: string;
  readonly os: string;
  readonly cpu: string;
  readonly libc: string | undefined;
  readonly runner: string;
  readonly cross: boolean;
}

export declare const NATIVE_PLATFORM_TARGETS: readonly NativePlatformTarget[];
export declare function nativeTargetByTriple(
  triple: string,
): NativePlatformTarget;
export declare function hostNativeTarget(): NativePlatformTarget;
