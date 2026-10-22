package main

import (
	"math"
	"math/rand"
	"testing"
)

// synthetic builds probes whose load time is a known function of the seeder
// count and whether the metadata was cold, plus noise. If the network cannot
// learn this it cannot learn anything.
func synthetic(n int, seed int64) []Sample {
	rng := rand.New(rand.NewSource(seed))
	out := make([]Sample, n)
	for i := range out {
		seeders := int(math.Exp(rng.Float64() * 6))
		size := int64(rng.Float64()*20) << 30
		cold := 0.0
		if rng.Float64() < 0.5 {
			cold = 1
		}
		y := 2.2 - 0.22*math.Log1p(float64(seeders)) + 0.75*cold
		y += rng.NormFloat64() * 0.2
		if y < 0.05 {
			y = 0.05
		}
		x := make([]float32, mInNum)
		x[0] = float32(math.Log1p(float64(seeders)))
		x[1] = float32(math.Log1p(float64(size)))
		x[8] = float32(cold)
		x[12] = float32(rng.NormFloat64() * 0.1)
		x[13] = 0.9
		out[i] = Sample{X: x, CatIx: rng.Intn(mIxBuckets), CatGrp: rng.Intn(mGrpBuckets), Y: y, Weight: 1}
	}
	return out
}

func TestMLPLearnsSyntheticRelation(t *testing.T) {
	samples := synthetic(900, 7)
	n, err := TrainMLP(samples, 1, 200)
	if err != nil {
		t.Fatalf("train: %v", err)
	}
	if n.MAE > 6 {
		t.Errorf("held-out MAE = %.2fs, want under 6s", n.MAE)
	}
	if n.Within2x < 0.65 {
		t.Errorf("within 2x = %.2f, want >= 0.65", n.Within2x)
	}
	// The metric that matters: the network has to beat predicting one number
	// for every release, or it is only adding a colour and a lie.
	if n.ConstantMAE <= 0 {
		t.Fatal("constant baseline was never measured")
	}
	if n.MAE >= n.ConstantMAE {
		t.Errorf("held-out MAE %.1fs does not beat the constant baseline %.1fs", n.MAE, n.ConstantMAE)
	}
}

// A fast swarm must not be predicted slower than a slow one: that ordering is
// the only thing the row actually needs to communicate.
func TestMLPOrdersFastAboveSlow(t *testing.T) {
	n, err := TrainMLP(synthetic(900, 7), 1, 200)
	if err != nil {
		t.Fatalf("train: %v", err)
	}
	fast := make([]float32, mInNum)
	fast[0] = float32(math.Log1p(600))
	fast[1] = float32(math.Log1p(8 << 30))
	slow := make([]float32, mInNum)
	slow[0] = float32(math.Log1p(2))
	slow[1] = float32(math.Log1p(8 << 30))
	slow[8] = 1 // cold, too

	pf, _ := n.Predict(fast, 2, 3)
	ps, _ := n.Predict(slow, 2, 3)
	if pf >= ps {
		t.Errorf("predicted fast=%.1fs slow=%.1fs — the relation is inverted", pf, ps)
	}
}

func TestMLPRoundTripsThroughJSON(t *testing.T) {
	n, err := TrainMLP(synthetic(300, 3), 5, 40)
	if err != nil {
		t.Fatalf("train: %v", err)
	}
	b, err := n.Marshal()
	if err != nil {
		t.Fatalf("marshal: %v", err)
	}
	back, err := UnmarshalMLP(b)
	if err != nil {
		t.Fatalf("unmarshal: %v", err)
	}
	x := make([]float32, mInNum)
	x[0] = 2
	a, b2 := n.Predict(x, 4, 9)
	c, d := back.Predict(x, 4, 9)
	if math.Abs(a-c) > 1e-9 || math.Abs(b2-d) > 1e-9 {
		t.Errorf("prediction changed across JSON: %.6f/%.6f vs %.6f/%.6f", a, b2, c, d)
	}
}

func TestUnmarshalRejectsWrongShape(t *testing.T) {
	if _, err := UnmarshalMLP([]byte(`{"mean":[1,2],"std":[1,2],"p":[1,2,3]}`)); err == nil {
		t.Fatal("accepted a model with the wrong number of parameters")
	}
}

func TestTrainRefusesThinData(t *testing.T) {
	if _, err := TrainMLP(synthetic(5, 1), 1, 10); err == nil {
		t.Fatal("trained on five samples")
	}
}

func TestFeaturesReadTheName(t *testing.T) {
	cases := []struct {
		name string
		res  int
		tier int
		pack bool
		grp  string
	}{
		{"The.Matrix.1999.2160p.UHD.BluRay.REMUX.x265-FLUX", 2160, 3, false, ""},
		{"The Matrix (1999) 1080p BrRip x264 - YIFY", 1080, 3, false, "YIFY"},
		{"Breaking.Bad.S03.1080p.WEB-DL.x264-GROUP", 1080, 2, true, "GROUP"},
		{"[SubsPlease] Frieren - 12 (1080p) [ABCD1234].mkv", 1080, 1, false, "SubsPlease"},
		{"Some Movie 2024 CAM x264", 0, 0, false, ""},
	}
	for _, c := range cases {
		if got := resolutionRung(c.name); got != c.res {
			t.Errorf("%q: resolution %d, want %d", c.name, got, c.res)
		}
		if got := sourceTier(c.name); got != c.tier {
			t.Errorf("%q: tier %d, want %d", c.name, got, c.tier)
		}
		if got := isPackName(c.name); got != c.pack {
			t.Errorf("%q: pack %v, want %v", c.name, got, c.pack)
		}
		if c.grp != "" {
			if got := releaseGroup(c.name); got != c.grp {
				t.Errorf("%q: group %q, want %q", c.name, got, c.grp)
			}
		}
	}
}

func TestFeatureVectorIsFinite(t *testing.T) {
	n, _ := TrainMLP(synthetic(200, 2), 3, 20)
	e := entryFacts{Title: "The.Matrix.1999.1080p.BluRay.x264-GROUP", Size: 8 << 30, Seeders: 120, Indexer: "apibay", Cold: true, Typ: "movie"}
	x, ix, grp := n.Features(e, 1_700_000_000)
	if len(x) != mInNum {
		t.Fatalf("feature length %d, want %d", len(x), mInNum)
	}
	for i, v := range x {
		if math.IsNaN(float64(v)) || math.IsInf(float64(v), 0) {
			t.Errorf("feature %d is not finite: %v", i, v)
		}
	}
	if ix <= 0 || ix >= mIxBuckets {
		t.Errorf("indexer bucket %d out of range", ix)
	}
	if grp <= 0 || grp >= mGrpBuckets {
		t.Errorf("group bucket %d out of range", grp)
	}
}

// The same indexer string must land in the same bucket every build, or the
// learned embedding migrates whenever Go's hash seed changes.
func TestIndexerBucketsAreStable(t *testing.T) {
	if a, b := bucketIndexer("apibay"), bucketIndexer("apibay"); a != b {
		t.Fatalf("apibay hashed to %d then %d", a, b)
	}
	if bucketIndexer("knaben") == bucketIndexer("apibay") {
		t.Fatal("known indexers collided")
	}
	if bucketIndexer("") != 0 {
		t.Fatal("the empty indexer must own bucket 0")
	}
}
