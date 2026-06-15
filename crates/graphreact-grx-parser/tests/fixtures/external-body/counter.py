# PCBPnR Phase 5-D follow-on fixture — external Python body for
# `<Counter src="./counter.py" lang="python"/>`.

def init(ctx):
    initial = ctx.props.get('initialCount', 0)
    count = use_state(initial)
    emit = use_emit()

    def on_increment(_msg):
        count.set(count.get() + 1)
    use_message('increment', on_increment)

    def fire_update():
        emit('count-updated', {'nodeId': ctx.id, 'value': count.get()})
    use_effect(fire_update, [count.get()])

    return {'accepts': ['increment'], 'emits': ['count-updated']}
