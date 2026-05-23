from __future__ import annotations

from agents.drafter_agents import (
    _build_minimal_traceability_report,
    _repair_traceability_min_fields,
)


def test_repair_traceability_fills_missing_claim_reports() -> None:
    claims = {
        "claims": [
            {"claim_number": 1, "elements": ["特征A"]},
            {"claim_number": 2, "elements": ["特征B"]},
        ]
    }
    half_structured = {
        "reports": [
            {
                "claim_number": 1,
                "elements_evidence": [],
                "is_fully_supported": False,
            }
        ],
        "overall_risk_assessment": "too short",
    }

    repaired = _repair_traceability_min_fields(half_structured, claims)
    assert len(repaired["reports"]) == 2
    assert repaired["reports"][0]["elements_evidence"]
    assert repaired["reports"][1]["elements_evidence"]
    assert len(repaired["overall_risk_assessment"]) >= 20


def test_build_minimal_traceability_report_is_schema_valid() -> None:
    claims = {"claims": [{"claim_number": 1, "elements": ["特征A"]}]}
    fallback = _build_minimal_traceability_report(claims, "parse failed")
    assert len(fallback["reports"]) >= 1
    assert fallback["reports"][0]["claim_number"] == 1
    assert fallback["reports"][0]["elements_evidence"][0]["support_level"] == "Unsupported"
    assert len(fallback["overall_risk_assessment"]) >= 20
