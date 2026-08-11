package twin

import "testing"

func TestHealthyScenarioClosesNormally(t *testing.T) {
	sim := NewSimulator(ScenarioHealthy)
	for range 4 {
		sim.Step()
	}
	if sim.ValveOpen || sim.Alarm || sim.Outcome != OutcomeHealthy {
		t.Fatalf("healthy cycle did not close normally: %#v", sim)
	}
	if sim.Moisture != 36 {
		t.Fatalf("moisture = %v, want 36", sim.Moisture)
	}
}

func TestStuckScenarioIsContainedAtExactSafetyLimit(t *testing.T) {
	sim := NewSimulator(ScenarioStuck)
	for range 4 {
		sim.Step()
	}
	if !sim.ValveOpen || !sim.CloseIgnored || sim.Alarm {
		t.Fatalf("fault should be active before cutoff: %#v", sim)
	}
	sim.Step()
	if sim.ValveOpen || !sim.Alarm || sim.Outcome != OutcomeContained {
		t.Fatalf("safety cutoff failed: %#v", sim)
	}
}

func TestSimulatorIsDeterministicAndResettable(t *testing.T) {
	a := NewSimulator(ScenarioStuck)
	b := NewSimulator(ScenarioStuck)
	for range 6 {
		a.Step()
		b.Step()
	}
	if a != b {
		t.Fatalf("same scenario diverged: %#v != %#v", a, b)
	}
	a = NewSimulator(ScenarioHealthy)
	if a.Tick != 0 || a.Moisture != InitialMoisture || a.ValveOpen || a.Alarm {
		t.Fatalf("reset baseline is unsafe: %#v", a)
	}
}

func TestProjectionCopiesReportsAndBoundsEvents(t *testing.T) {
	p := NewProjection()
	state := map[string]any{"tick": 2.0, "open": true}
	p.Report("valve", state)
	state["open"] = false
	for i := 0; i < EventHistoryLimit+5; i++ {
		p.AddEvent(DomainEvent{Kind: "test"})
	}
	snapshot := p.Snapshot()
	if snapshot.Nodes["valve"]["open"] != true {
		t.Fatal("projection retained caller-owned map")
	}
	if len(snapshot.Events) != EventHistoryLimit {
		t.Fatalf("events = %d, want %d", len(snapshot.Events), EventHistoryLimit)
	}
}
