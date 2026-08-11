package twin

import (
	"fmt"
	"sort"
	"sync"
	"sync/atomic"
)

const (
	SchemaVersion     = 1
	InitialMoisture   = 42.0
	DryThreshold      = 30.0
	CloseThreshold    = 36.0
	MaximumOpenTicks  = 3
	MinutesPerTick    = 5
	ScenarioHealthy   = "healthy"
	ScenarioStuck     = "stuck_irrigation"
	OutcomeHealthy    = "healthy"
	OutcomeAtRisk     = "at_risk"
	OutcomeContained  = "contained"
	EventHistoryLimit = 24
)

var validScenarios = map[string]struct{}{
	ScenarioHealthy: {},
	ScenarioStuck:   {},
}

func ValidScenario(value string) bool {
	_, ok := validScenarios[value]
	return ok
}

func NextMoisture(current float64, valveOpen bool) float64 {
	if valveOpen {
		current += 4
	} else {
		current -= 7
	}
	if current < 0 {
		return 0
	}
	if current > 100 {
		return 100
	}
	return current
}

func Number(value any, fallback float64) float64 {
	if n, ok := value.(float64); ok {
		return n
	}
	return fallback
}

func Boolean(value any, fallback bool) bool {
	if b, ok := value.(bool); ok {
		return b
	}
	return fallback
}

func String(value any, fallback string) string {
	if s, ok := value.(string); ok {
		return s
	}
	return fallback
}

func Payload(message map[string]any) map[string]any {
	payload, _ := message["payload"].(map[string]any)
	if payload == nil {
		return map[string]any{}
	}
	return payload
}

func CopyMap(source map[string]any) map[string]any {
	copy := make(map[string]any, len(source))
	for key, value := range source {
		copy[key] = value
	}
	return copy
}

type DomainEvent struct {
	Tick    int    `json:"tick"`
	Node    string `json:"node"`
	Kind    string `json:"kind"`
	Message string `json:"message"`
}

type Snapshot struct {
	SchemaVersion int                       `json:"schema_version"`
	Scenario      string                    `json:"scenario"`
	Tick          int                       `json:"tick"`
	SimMinutes    int                       `json:"sim_minutes"`
	Running       bool                      `json:"running"`
	Outcome       string                    `json:"outcome"`
	Nodes         map[string]map[string]any `json:"nodes"`
	Risk          map[string]any            `json:"risk"`
	Events        []DomainEvent             `json:"recent_domain_events"`
}

type Projection struct {
	mu       sync.RWMutex
	scenario string
	tick     int
	running  bool
	outcome  string
	nodes    map[string]map[string]any
	risk     map[string]any
	events   []DomainEvent
}

func NewProjection() *Projection {
	p := &Projection{}
	p.Reset()
	return p
}

func (p *Projection) Reset() {
	p.mu.Lock()
	defer p.mu.Unlock()
	p.scenario = ScenarioHealthy
	p.tick = 0
	p.running = false
	p.outcome = OutcomeHealthy
	p.nodes = map[string]map[string]any{}
	p.risk = map[string]any{}
	p.events = []DomainEvent{{Tick: 0, Node: "twin-gateway", Kind: "reset", Message: "Twin reset to a safe baseline"}}
}

func (p *Projection) SetScenario(scenario string) error {
	if !ValidScenario(scenario) {
		return fmt.Errorf("unknown scenario %q", scenario)
	}
	p.mu.Lock()
	defer p.mu.Unlock()
	p.scenario = scenario
	p.outcome = OutcomeHealthy
	p.events = appendBounded(p.events, DomainEvent{Tick: p.tick, Node: "twin-gateway", Kind: "scenario", Message: "Scenario selected: " + scenario})
	return nil
}

func (p *Projection) SetClock(running bool) {
	p.mu.Lock()
	defer p.mu.Unlock()
	p.running = running
}

func (p *Projection) Report(node string, state map[string]any) {
	p.mu.Lock()
	defer p.mu.Unlock()
	p.nodes[node] = CopyMap(state)
	if tick, ok := state["tick"].(float64); ok && int(tick) > p.tick {
		p.tick = int(tick)
	}
	if outcome, ok := state["outcome"].(string); ok && outcome != "" {
		p.outcome = outcome
	}
	if node == "risk-analyzer" {
		p.risk = CopyMap(state)
	}
}

func (p *Projection) AddEvent(event DomainEvent) {
	p.mu.Lock()
	defer p.mu.Unlock()
	if event.Tick == 0 {
		event.Tick = p.tick
	}
	p.events = appendBounded(p.events, event)
}

func (p *Projection) Snapshot() Snapshot {
	p.mu.RLock()
	defer p.mu.RUnlock()
	nodes := make(map[string]map[string]any, len(p.nodes))
	for id, state := range p.nodes {
		nodes[id] = CopyMap(state)
	}
	risk := CopyMap(p.risk)
	events := append([]DomainEvent(nil), p.events...)
	return Snapshot{
		SchemaVersion: SchemaVersion,
		Scenario:      p.scenario,
		Tick:          p.tick,
		SimMinutes:    p.tick * MinutesPerTick,
		Running:       p.running,
		Outcome:       p.outcome,
		Nodes:         nodes,
		Risk:          risk,
		Events:        events,
	}
}

func appendBounded(events []DomainEvent, event DomainEvent) []DomainEvent {
	events = append(events, event)
	if len(events) > EventHistoryLimit {
		events = append([]DomainEvent(nil), events[len(events)-EventHistoryLimit:]...)
	}
	return events
}

var projection = NewProjection()
var safetyLockout atomic.Bool

func Store() *Projection          { return projection }
func SafetyLockedOut() bool       { return safetyLockout.Load() }
func SetSafetyLockout(value bool) { safetyLockout.Store(value) }

// Simulator is the pure reference state machine used by unit and acceptance
// tests. Runtime nodes implement the same transitions through messages.
type Simulator struct {
	Scenario     string
	Tick         int
	Moisture     float64
	ValveOpen    bool
	OpenTicks    int
	CloseIgnored bool
	Alarm        bool
	Outcome      string
}

func NewSimulator(scenario string) Simulator {
	if !ValidScenario(scenario) {
		scenario = ScenarioHealthy
	}
	return Simulator{Scenario: scenario, Moisture: InitialMoisture, Outcome: OutcomeHealthy}
}

func (s *Simulator) Step() {
	s.Tick++
	wasOpen := s.ValveOpen
	s.Moisture = NextMoisture(s.Moisture, s.ValveOpen)
	if !s.ValveOpen && s.Moisture < DryThreshold {
		s.ValveOpen = true
	}
	// The interlock counts complete ticks spent open. The tick that issues the
	// open command starts the window but is not itself a completed open tick.
	if wasOpen && s.ValveOpen {
		s.OpenTicks++
	}
	if s.ValveOpen && s.Moisture >= CloseThreshold {
		if s.Scenario == ScenarioStuck {
			s.CloseIgnored = true
		} else {
			s.ValveOpen = false
			s.OpenTicks = 0
		}
	}
	if s.ValveOpen && s.OpenTicks >= MaximumOpenTicks {
		s.ValveOpen = false
		s.Alarm = true
		s.Outcome = OutcomeContained
		return
	}
	if s.ValveOpen && s.Scenario == ScenarioStuck {
		s.Outcome = OutcomeAtRisk
	} else {
		s.Outcome = OutcomeHealthy
	}
}

func SortedKeys(values map[string]map[string]any) []string {
	keys := make([]string, 0, len(values))
	for key := range values {
		keys = append(keys, key)
	}
	sort.Strings(keys)
	return keys
}
