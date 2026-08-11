# OpenAI Go SDK node

This example compiles the official OpenAI Go SDK directly into a
GraphReact Go node. Dependency versions and checksums are pinned by the
adjacent `go.mod` and `go.sum` files.

```sh
export GRX_SECRET_OPENAI_API_KEY='...'
grx run --runtime=go agent.grx
```

Starting the graph does not call OpenAI. The node calls the Responses
API only when it receives a `prompt` message. In a daemon, send one via
the default-deny ingress surface after declaring the same external
message contract shown in `agent.grx`.

Update the SDK lockfiles with standard Go tooling from this directory;
GraphReact always consumes them with readonly module semantics.
