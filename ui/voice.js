(() => {
  const normalize = (value) => String(value || "")
    .trim()
    .toLowerCase()
    .replace(/[.!?]+$/g, "")
    .replace(/\s+/g, " ");

  function parseCommand(transcript) {
    const text = normalize(transcript);
    if (!text) return null;

    if (/^(new|create|start) (chat|thread|conversation)$/.test(text)) {
      return { type: "new_thread" };
    }
    if (/^(stop|stop generation|stop response|cancel response)$/.test(text)) {
      return { type: "stop_generation" };
    }
    if (/^(retry|try again|regenerate|regenerate response)$/.test(text)) {
      return { type: "retry" };
    }
    if (/^(send|send draft|send message|send current draft)$/.test(text)) {
      return { type: "send_current_draft" };
    }

    const model = text.match(/^(?:switch to|use|select) model (.+)$/);
    if (model?.[1]?.trim()) {
      return { type: "set_model", query: model[1].trim() };
    }

    const artifact = text.match(/^(?:attach|select) (?:file|artifact) (.+)$/);
    if (artifact?.[1]?.trim()) {
      return { type: "attach_artifact", query: artifact[1].trim() };
    }

    return null;
  }

  const api = Object.freeze({ normalize, parseCommand });
  if (typeof module !== "undefined" && module.exports) module.exports = api;
  globalThis.LLMGatewayVoice = api;
})();
