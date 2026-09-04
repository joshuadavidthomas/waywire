// Trial-only instrumentation. Observe video headers; never retain H.264 data,
// input, clipboard text, URLs or credentials. The browser closes after the run.
(() => {
                    
                 
                     
                       
                         
   
                      
                  
                                                      
   
  const scope = window                                               ;
  if (scope.__rustVideoObserver) return;
  let startedAt = Infinity;
  let headers           = [];
  let overflow = 0;
  scope.__rustVideoObserver = {
    start() {
      headers = [];
      overflow = 0;
      startedAt = performance.now();
    },
    finish() {
      startedAt = Infinity;
      return { headers, overflow };
    },
  };
  const NativeWebSocket = window.WebSocket;
  window.WebSocket = class extends NativeWebSocket {
    constructor(url              , protocols                    ) {
      super(url, protocols);
      if (new URL(url, location.href).pathname !== "/stream") return;
      this.addEventListener("message", (event) => {
        const atMs = performance.now() - startedAt;
        if (atMs < 0 || atMs >= 30_000 || !(event.data instanceof ArrayBuffer))
          return;
        const bytes = event.data;
        if (bytes.byteLength < 40) return;
        const view = new DataView(bytes);
        if (view.getUint8(0) !== 2 || view.getUint8(1) !== 1) return;
        if (headers.length >= 10_000) {
          overflow += 1;
          return;
        }
        headers.push({
          atMs,
          sequence: view.getBigUint64(4, true).toString(),
          generation: view.getUint32(20, true),
          captureNanos: view.getBigUint64(28, true).toString(),
        });
      });
    }
  };
})();
