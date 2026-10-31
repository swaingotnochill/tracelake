"""A small inspect-ai task used to produce a real eval log for TraceLake.

Run it with any inspect model, for example:

    inspect eval task.py --model openai-api/zai/glm-4.7 \
        --model-base-url https://api.z.ai/api/paas/v4 \
        --limit 6 --log-dir logs

The task is deliberately simple. What matters for the TraceLake example is that
the log carries real samples, real model outputs, and real scorer values.
"""

from inspect_ai import Task, task
from inspect_ai.dataset import MemoryDataset, Sample
from inspect_ai.scorer import exact, includes
from inspect_ai.solver import generate

SAMPLES = [
    Sample(
        id="arith-1",
        input="What is 17 + 26? Reply with the number only.",
        target="43",
        metadata={"trace_id": "trace-arith-1", "topic": "arithmetic"},
    ),
    Sample(
        id="arith-2",
        input="What is 9 * 9? Reply with the number only.",
        target="81",
        metadata={"trace_id": "trace-arith-2", "topic": "arithmetic"},
    ),
    Sample(
        id="arith-3",
        input="What is 144 / 12? Reply with the number only.",
        target="12",
        metadata={"trace_id": "trace-arith-3", "topic": "arithmetic"},
    ),
    Sample(
        id="refund-1",
        input=(
            "A customer was charged twice for order 8837. In one sentence, say what "
            "the support agent should do first."
        ),
        target="refund",
        metadata={"trace_id": "trace-refund-1", "topic": "support"},
    ),
    Sample(
        id="refund-2",
        input=(
            "In one sentence, explain the first step for a customer whose refund has "
            "not arrived after ten business days."
        ),
        target="refund",
        metadata={"trace_id": "trace-refund-2", "topic": "support"},
    ),
    Sample(
        id="policy-1",
        input="Name the document a support agent must check before issuing a refund.",
        target="policy",
        metadata={"trace_id": "trace-policy-1", "topic": "support"},
    ),
]


@task
def tracelake_demo() -> Task:
    return Task(
        dataset=MemoryDataset(samples=SAMPLES, name="tracelake_demo"),
        solver=[generate()],
        scorer=[exact(), includes()],
    )
