(() => {
  const state = (globalThis.__socketLocal = {
    submitted: 0,
    decoded: 0,
    outputs: [],
    configs: [],
  });
  if (!globalThis.VideoDecoder) return;
  const Native = globalThis.VideoDecoder;
  globalThis.VideoDecoder = new Proxy(Native, {
    construct(Target, args) {
      const options = args[0];
      return new Target({
        ...options,
        output(frame) {
          state.decoded++;
          state.outputs.push({
            width: frame.displayWidth,
            height: frame.displayHeight,
            format: frame.format,
            colorSpace: frame.colorSpace,
          });
          if (state.outputs.length > 20) state.outputs.shift();
          options.output(frame);
        },
      });
    },
  });
  const decode = Native.prototype.decode;
  Native.prototype.decode = function (chunk) {
    state.submitted++;
    return decode.call(this, chunk);
  };
  const configure = Native.prototype.configure;
  Native.prototype.configure = function (config) {
    state.configs.push(config);
    return configure.call(this, config);
  };
})();
