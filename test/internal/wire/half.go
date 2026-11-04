package wire

import "math"

// HalfToFloat converts an IEEE-754 binary16 value to float32.
func HalfToFloat(h uint16) float32 {
	sign := uint32(h&0x8000) << 16
	exp := uint32(h>>10) & 0x1F
	mant := uint32(h & 0x03FF)

	switch {
	case exp == 0:
		if mant == 0 {
			return math.Float32frombits(sign) // +/- zero
		}
		// subnormal: normalise
		e := int32(-1)
		for mant&0x0400 == 0 {
			mant <<= 1
			e--
		}
		mant &= 0x03FF
		exp32 := uint32(int32(127-15) + e + 1)
		return math.Float32frombits(sign | exp32<<23 | mant<<13)
	case exp == 0x1F:
		if mant == 0 {
			return math.Float32frombits(sign | 0x7F800000) // +/- inf
		}
		return math.Float32frombits(sign | 0x7F800000 | mant<<13) // NaN
	default:
		exp32 := exp + (127 - 15)
		return math.Float32frombits(sign | exp32<<23 | mant<<13)
	}
}

// FloatToHalf converts a float32 to IEEE-754 binary16, truncating the mantissa.
func FloatToHalf(f float32) uint16 {
	bits := math.Float32bits(f)
	sign := uint16((bits >> 16) & 0x8000)
	exp := int32((bits>>23)&0xFF) - 127 + 15
	mant := bits & 0x7FFFFF

	switch {
	case (bits & 0x7FFFFFFF) == 0:
		return sign // zero
	case ((bits >> 23) & 0xFF) == 0xFF:
		if mant != 0 {
			return sign | 0x7E00 // NaN
		}
		return sign | 0x7C00 // inf
	case exp >= 0x1F:
		return sign | 0x7C00 // overflow -> inf
	case exp <= 0:
		if exp < -10 {
			return sign // underflow -> zero
		}
		mant |= 0x800000
		shift := uint32(14 - exp)
		return sign | uint16(mant>>shift)
	default:
		return sign | uint16(exp)<<10 | uint16(mant>>13)
	}
}
