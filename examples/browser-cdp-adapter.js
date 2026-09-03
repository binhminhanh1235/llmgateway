// Trusted local example for llmgateway v0.16 browser-cdp.
//
// This file intentionally does not target a real provider. A provider plugin may use the
// authenticated page's normal DOM or same-origin fetch capabilities, but it must respect
// that provider's terms and must not bypass CAPTCHA, 2FA, anti-abuse controls, or quotas.

globalThis.__LLMGATEWAY_ADAPTER__ = {
  async chat(request) {
    const lastUserMessage = [...(request.messages || [])]
      .reverse()
      .find((message) => message.role === "user");

    const content = typeof lastUserMessage?.content === "string"
      ? lastUserMessage.content
      : "";

    if (request.stream === true) {
      const chunk = {
        id: "chatcmpl_browser_cdp_example",
        object: "chat.completion.chunk",
        model: request.model,
        choices: [
          {
            index: 0,
            delta: { role: "assistant", content: `CDP example: ${content}` },
            finish_reason: "stop"
          }
        ]
      };

      return {
        status: 200,
        content_type: "text/event-stream",
        body: `data: ${JSON.stringify(chunk)}\n\ndata: [DONE]\n\n`
      };
    }

    return {
      status: 200,
      content_type: "application/json",
      body: {
        id: "chatcmpl_browser_cdp_example",
        object: "chat.completion",
        model: request.model,
        choices: [
          {
            index: 0,
            message: {
              role: "assistant",
              content: `CDP example: ${content}`
            },
            finish_reason: "stop"
          }
        ],
        usage: {
          prompt_tokens: 0,
          completion_tokens: 0,
          total_tokens: 0
        }
      }
    };
  }
};
