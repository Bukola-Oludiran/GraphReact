# PCBPnR Phase 5-D follow-on item #2 — external Python body referenced
# from python-external-body.grx via <Counter src="./python-external-body.py"
# lang="python"/>. Validates the end-to-end mount path: parser AST →
# multi-file resolver reads this file → IR carries the body → pyruntime
# evals it in a CPython sub-interpreter.

def body(ctx):
    return {
        'id': ctx['id'],
        'kind': 'external-counter',
        'accepts': ['increment'],
        'emits': ['count-updated'],
        'loaded_from': 'external_py_file',
    }
