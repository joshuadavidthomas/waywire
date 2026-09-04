/// <reference path="../worker-configuration.d.ts" />

export {};

declare global {
  namespace App {
    interface Platform {
      env: Env;
      cf: IncomingRequestCfProperties;
      ctx: ExecutionContext;
      caches: CacheStorage;
    }
  }
}
