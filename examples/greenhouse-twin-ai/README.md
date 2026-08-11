# Optional greenhouse narrator

This separate GraphReact system keeps the core greenhouse twin deterministic
and standard-library-only. It accepts a sanitized snapshot and returns a
read-only explanation; it has no edge or message route back to twin controls.

Set the API key through GraphReact's secret store and choose the model
explicitly at launch:

```sh
grx secrets set OPENAI_API_KEY
export OPENAI_MODEL=<responses-compatible-model-id>
grx cluster daemon --dev-loopback --path narrator.grx --runtime go \
  --observer-bind 127.0.0.1:9091
```

Compilation and tests never make an OpenAI request.
