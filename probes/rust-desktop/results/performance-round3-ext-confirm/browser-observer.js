// Trial-only instrumentation for the known sprite-desktop performance viewer.
// Other sessions and WebCodecs users are unsupported. Retain timing and
// video-header metadata only: no encoded payload, input, clipboard text, URLs,
// or credentials.
(() => {
                    
                 
                     
                       
                                 
                         
                  
                    
   
                         
                 
                                 
   
                                                  
                    
   
                          
                 
                   
   
                      
                 
                       
   
                      
                    
                              
                           
                   
                      
   
                         
                                                
                                        
                      
                                          
                                  
                           
                          
                       
   
                      
                  
                          
   

  const scope = window                                               ;
  if (scope.__rustVideoObserver) return;

  const limits = {
    headers: 10_000,
    decodeSubmissions: 10_000,
    decoderOutputs: 10_000,
    resets: 1_000,
    longTasks: 1_000,
  }         ;
  let startedAt = Infinity;
  let headers           = [];
  let decodeSubmissions                     = [];
  let decoderOutputs                = [];
  let resets                 = [];
  let longTasks             = [];
  let overflow           = emptyOverflow();

  function emptyOverflow()           {
    return {
      headers: 0,
      decodeSubmissions: 0,
      decoderOutputs: 0,
      resets: 0,
      longTasks: 0,
    };
  }

  function elapsed()         {
    return performance.now() - startedAt;
  }

  function inWindow(atMs        )          {
    return atMs >= 0 && atMs < 30_000;
  }

  function append                             (
    name   ,
    values     ,
    value   ,
  )       {
    if (values.length < limits[name]) values.push(value);
    else overflow[name] += 1;
  }

  const collectLongTasks = (entries                             ) => {
    for (const entry of entries) {
      const atMs = entry.startTime - startedAt;
      if (inWindow(atMs)) {
        append("longTasks", longTasks, { atMs, durationMs: entry.duration });
      }
    }
  };
  let longTaskObserver                                 ;
  if (
    typeof PerformanceObserver !== "undefined" &&
    PerformanceObserver.supportedEntryTypes.includes("longtask")
  ) {
    longTaskObserver = new PerformanceObserver((list) => {
      collectLongTasks(list.getEntries());
    });
    longTaskObserver.observe({ type: "longtask" });
  }

  if (typeof VideoDecoder !== "undefined") {
    const NativeVideoDecoder = VideoDecoder;
    const nativeReset = NativeVideoDecoder.prototype.reset;
    const nativeDecode = NativeVideoDecoder.prototype.decode;
    NativeVideoDecoder.prototype.reset = function () {
      const atMs = elapsed();
      if (inWindow(atMs)) {
        append("resets", resets, { atMs, queued: this.decodeQueueSize });
      }
      return nativeReset.call(this);
    };
    NativeVideoDecoder.prototype.decode = function (chunk) {
      const atMs = elapsed();
      if (inWindow(atMs)) {
        append("decodeSubmissions", decodeSubmissions, {
          atMs,
          mediaTimestampMicros: String(chunk.timestamp),
          auBytes: chunk.byteLength,
        });
      }
      return nativeDecode.call(this, chunk);
    };
    const InstrumentedVideoDecoder = new Proxy(NativeVideoDecoder, {
      construct(target, args, newTarget) {
        const init = args[0]                    ;
        const output = init.output;
        const wrapped                   = {
          ...init,
          output(frame) {
            const atMs = elapsed();
            if (inWindow(atMs)) {
              append("decoderOutputs", decoderOutputs, {
                atMs,
                mediaTimestampMicros: String(frame.timestamp),
              });
            }
            output(frame);
          },
        };
        return Reflect.construct(target, [wrapped], newTarget);
      },
    });
    window.VideoDecoder = InstrumentedVideoDecoder;
  }

  const NativeWebSocket = window.WebSocket;
  const InstrumentedWebSocket = new Proxy(NativeWebSocket, {
    construct(target, args, newTarget) {
      const socket = Reflect.construct(target, args, newTarget)             ;
      const url = args[0];
      if (
        (typeof url === "string" || url instanceof URL) &&
        new URL(url, location.href).pathname === "/stream"
      ) {
        socket.addEventListener("message", (event) => {
          const atMs = elapsed();
          if (
            !inWindow(atMs) ||
            !(event.data instanceof ArrayBuffer) ||
            event.data.byteLength < 40
          )
            return;
          const view = new DataView(event.data);
          if (view.getUint8(0) !== 2 || view.getUint8(1) !== 1) return;
          append("headers", headers, {
            atMs,
            sequence: view.getBigUint64(4, true).toString(),
            mediaTimestampMicros: view.getBigUint64(12, true).toString(),
            generation: view.getUint32(20, true),
            captureNanos: view.getBigUint64(28, true).toString(),
            flags: view.getUint8(2),
            auBytes: event.data.byteLength - 40,
          });
        });
      }
      return socket;
    },
  });
  window.WebSocket = InstrumentedWebSocket;

  function assertRecordingStarted(value        )       {
    if (!Number.isFinite(value) || value < 0) {
      throw new Error("Browser timing observation was not started");
    }
  }

  scope.__rustVideoObserver = {
    start() {
      headers = [];
      decodeSubmissions = [];
      decoderOutputs = [];
      resets = [];
      longTasks = [];
      overflow = emptyOverflow();
      startedAt = performance.now();
    },
    finish() {
      collectLongTasks(longTaskObserver?.takeRecords() ?? []);
      const recordingPerformanceStartMs = startedAt;
      assertRecordingStarted(recordingPerformanceStartMs);
      startedAt = Infinity;
      longTaskObserver?.disconnect();
      return {
        schema: "sprite-desktop-browser-timings-v2",
        recordingPerformanceStartMs,
        headers,
        decodeSubmissions,
        decoderOutputs,
        resets,
        longTasks,
        overflow,
      };
    },
  };
})();
