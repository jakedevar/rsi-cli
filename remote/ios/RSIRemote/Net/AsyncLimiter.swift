import Foundation

/// A first-come, first-served counting semaphore for async callers. A caller
/// cancelled while it waits leaves the queue with `CancellationError` and
/// never holds a slot.
actor AsyncLimiter {
    private var available: Int
    private var waiters: [(id: UInt64, continuation: CheckedContinuation<Void, any Error>)] = []
    private var nextId: UInt64 = 0

    init(slots: Int) {
        precondition(slots > 0)
        available = slots
    }

    /// Wait for a slot. Every successful `acquire` needs exactly one `release`.
    func acquire() async throws {
        if available > 0, waiters.isEmpty {
            available -= 1
            return
        }
        let id = nextId
        nextId += 1
        try await withTaskCancellationHandler {
            try await withCheckedThrowingContinuation { (continuation: CheckedContinuation<Void, any Error>) in
                if Task.isCancelled {
                    continuation.resume(throwing: CancellationError())
                } else {
                    waiters.append((id, continuation))
                }
            }
        } onCancel: {
            Task { await self.withdraw(id) }
        }
    }

    /// Hand the slot to the longest waiter, or put it back.
    func release() {
        if waiters.isEmpty {
            available += 1
        } else {
            waiters.removeFirst().continuation.resume()
        }
    }

    private func withdraw(_ id: UInt64) {
        guard let index = waiters.firstIndex(where: { $0.id == id }) else { return }
        waiters.remove(at: index).continuation.resume(throwing: CancellationError())
    }
}
