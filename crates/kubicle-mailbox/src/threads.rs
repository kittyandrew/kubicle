//! Threads as Roundcube shows them, from Dovecot's THREAD=REFS: a thread sits where its first message falls in
//! the requested sort, and replies follow their parent oldest first. A missing root with several replies (every
//! GitHub notification thread: GitHub never sends the mail they all reference) gives way to its oldest reply
//! instead of splitting the thread.

use std::collections::HashMap;

use kubicle_imap::Thread;

/// One message's place in the threaded list.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Threaded {
    pub uid: u32,
    pub depth: u16,
    /// Messages below this one in its thread.
    pub replies: u32,
}

/// The messages in thread order. `sorted` holds every UID of the threads in the requested sort order; `nodes` is the
/// THREAD answer, flat, each node after its parent. Each step is a pass over the nodes, none of them recursive.
pub fn order(nodes: &[Thread], sorted: &[u32]) -> Vec<Threaded> {
    let place: HashMap<u32, usize> = sorted.iter().enumerate().map(|(i, &uid)| (uid, i)).collect();
    let (mut children, mut roots) = (vec![Vec::new(); nodes.len()], Vec::new());
    for (i, node) in nodes.iter().enumerate() {
        match node.parent {
            Some(parent) => children[parent].push(i),
            None => roots.push(i),
        }
    }
    // The oldest message under each node, UIDs growing with arrival: replies sort by it, oldest first.
    let mut oldest: Vec<u32> = nodes.iter().map(|n| n.uid.unwrap_or(u32::MAX)).collect();
    for i in (0..nodes.len()).rev() {
        if let Some(parent) = nodes[i].parent {
            oldest[parent] = oldest[parent].min(oldest[i]);
        }
    }
    for list in &mut children {
        list.sort_by_key(|&c| oldest[c]);
    }
    let mut threads: Vec<(usize, Vec<Threaded>)> = roots
        .into_iter()
        .filter_map(|mut root| {
            // A root the mailbox lacks gives way to its oldest reply, which takes the others as replies.
            while nodes[root].uid.is_none() {
                let mut replies = std::mem::take(&mut children[root]).into_iter();
                let heir = replies.next()?;
                children[heir].extend(replies);
                children[heir].sort_by_key(|&c| oldest[c]);
                root = heir;
            }
            // Depth first, replies oldest first; a message missing inside the thread hands its replies to its parent.
            let (mut rows, mut stack) = (Vec::new(), vec![(root, 0u16)]);
            while let Some((node, depth)) = stack.pop() {
                let below = nodes[node].uid.map_or(depth, |uid| {
                    rows.push(Threaded { uid, depth, replies: 0 });
                    depth.saturating_add(1)
                });
                stack.extend(children[node].iter().rev().map(|&c| (c, below))); // reversed: the oldest pops first
            }
            count_replies(&mut rows);
            Some((rows.iter().filter_map(|r| place.get(&r.uid)).copied().min().unwrap_or(usize::MAX), rows))
        })
        .collect();
    threads.sort_by_key(|(first, _)| *first); // where its first message falls in the requested sort
    threads.into_iter().flat_map(|(_, rows)| rows).collect()
}

/// Each row's replies: the rows after it deeper than it, up to the next at its depth or above. One pass with a stack
/// of the rows still open, where counting forward from each row was quadratic in a reply chain.
fn count_replies(rows: &mut [Threaded]) {
    let mut open: Vec<usize> = Vec::new();
    for i in 0..=rows.len() {
        let depth = rows.get(i).map(|r| r.depth);
        while let Some(&o) = open.last().filter(|&&o| depth.is_none_or(|d| rows[o].depth >= d)) {
            rows[o].replies = u32::try_from(i - o - 1).unwrap_or(u32::MAX);
            open.pop();
        }
        if depth.is_some() {
            open.push(i);
        }
    }
}
