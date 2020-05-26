use tari_broadcast_channel::raw_bounded;

fn main() {
    let (tx, rx) = raw_bounded(10, 1);
    (1..15).for_each(|x| tx.broadcast(x).unwrap());

    let rx2 = rx.clone();

    let received: Vec<i32> = rx.map(|x| *x).collect();
    // Test that only the last 10 elements are in the received list.
    let expected: Vec<i32> = (5..15).collect();

    assert_eq!(expected, received);

    // Test messages discarded
    assert_eq!(rx2.get_dropped_messages_state(), false);
    rx2.try_recv().unwrap();
    assert_eq!(rx2.get_dropped_messages_count(), 4);
    assert_eq!(rx2.get_dropped_messages_state(), true);
    rx2.try_recv().unwrap();
    assert_eq!(rx2.get_dropped_messages_count(), 4);
    assert_eq!(rx2.get_dropped_messages_state(), false);
}
