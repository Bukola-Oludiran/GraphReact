// PCBPnR Phase 5-D follow-on item #2 — external JS body referenced
// from js-external-body.grx via <Counter src="./js-external-body.js"
// lang="js"/>. Validates the parser → resolver → IR → JSC sub-isolate
// execution path. JS body shape mirrors inline-body convention: the
// body is a function-body expression that returns the component's
// permission manifest.

useState(0);
return {
    id: ctx.id,
    kind: 'external-counter-js',
    accepts: ['increment'],
    emits: ['count-updated'],
    loaded_from: 'external_js_file',
};
