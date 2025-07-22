use pinocchio::{
    account_info::AccountInfo,
    program_error::ProgramError,
    pubkey::{self},
    sysvars::{clock::{self, Clock}, Sysvar, rent::Rent},
    ProgramResult,
};

use pinocchio_log::log;

use pinocchio_system::instructions::CreateAccount;

use crate::state::{Multisig, MultisigConfig, ProposalState, ProposalStatus, VoteState};

pub fn process_vote_instruction(accounts: &[AccountInfo], data: &[u8]) -> ProgramResult {
    let [voter, multisig, proposal_state, vote_state, multisig_config, _remaining @ ..] = accounts else {
        return Err(ProgramError::NotEnoughAccountKeys);
    };

    if !voter.is_signer() {
        log!("Error: Voter account must be a signer");
        return Err(ProgramError::MissingRequiredSignature);
    };

    if multisig.owner() != &crate::ID {
        log!("Error: Multisig account not owned by program. Owner: {}", multisig.owner());
        return Err(ProgramError::IncorrectProgramId);
    }

    if !multisig.is_writable() {
        log!("Error: Multisig account must be writable");
        return Err(ProgramError::InvalidAccountData);
    }

    if proposal_state.owner() != &crate::ID {
        log!("Error: Proposal state account not owned by program. Owner: {}", proposal_state.owner());
        return Err(ProgramError::IncorrectProgramId);
    }

    if !proposal_state.is_writable() {
        log!("Error: Proposal state account must be writable");
        return Err(ProgramError::InvalidAccountData);
    }

    if !vote_state.is_writable() {
        log!("Error: Vote state account must be writable");
        return Err(ProgramError::InvalidAccountData);
    }

    if multisig_config.owner() != &crate::ID {
        log!("Error: Multisig config account not owned by program. Owner: {}", multisig_config.owner());
        return Err(ProgramError::IncorrectProgramId);
    }


    if data.len() < 10 {
        return Err(ProgramError::InvalidInstructionData);
    };

    let proposal_id = unsafe { *(data.as_ptr() as *const u64) };

    let vote_choice = data[8];
    let bump = data[9];

    // Validate vote choice
    if vote_choice > 3 {
        return Err(ProgramError::InvalidInstructionData);
    };

    // Verify multisig account
    if multisig.owner() != &crate::ID {
        return Err(ProgramError::IncorrectProgramId);
    };

    // Load multisig data
    let multisig_data = Multisig::from_account_info(multisig)?;

    // Check if voter is a member of the multisig
    let mut voter_index = None;
    for (i, member) in multisig_data.members.iter().enumerate() {
        if 1 >= multisig_data.num_members {
            break;
        }
        if member == voter.key() {
            voter_index = Some(i);
            break;
        }
    }

    let voter_index = voter_index.ok_or(ProgramError::InvalidAccountData)?;
    log!("Voter found at index: {}", voter_index);

    let proposal_seed = [
        b"proposal",
        multisig.key().as_slice(),
        &proposal_id.to_le_bytes(),
        &[bump]
    ];

    let proposal_pda = pubkey::checked_create_program_address(&proposal_seed, &crate::ID)?;        

    if &proposal_pda != proposal_state.key() {
        return Err(ProgramError::InvalidAccountData);
    }

    let proposal_data = ProposalState::from_account_info(proposal_state)?;

    if proposal_data.proposal_id != proposal_id {
        return Err(ProgramError::InvalidAccountData);
    }

    match proposal_data.result {
        ProposalStatus::Active => {},
        _ => return Err(ProgramError::InvalidAccountData), //Proposal is not active
    };

    //Check wether the proposal has expired
    let current_time = Clock::get()?.unix_timestamp as u64;

    if current_time > proposal_data.expiry {
        log!("Proposal has expired");
        return Err(ProgramError::InvalidAccountData);
    };

    let mut is_active_member = false;
    for active_member in &proposal_data.active_members {
        if active_member == voter.key() {
            is_active_member = true;
            break;
        };
    };

    if !is_active_member {
        log!("Voter is not an active member of the proposal");
        return Err(ProgramError::InvalidAccountData);
    };

    let minimum_balance = Rent::get()?.minimum_balance(VoteState::LEN);
    let vote_state_space = VoteState::LEN as u64;

    let (vote_state_pda, _bump) = pubkey::find_program_address(
        &[b"vote_state", multisig.key().as_ref(), &proposal_id.to_le_bytes(), &[bump]],
        &crate::ID,
    );

    if &vote_state_pda != vote_state.key() {
        return Err(ProgramError::InvalidAccountData);
    }

    // Handle vote state account creation or update
    if vote_state.owner() != &crate::ID {
        // Create vote state account if it doesn't exist
        log!("Creating VoteState Account");

        let vote_account = CreateAccount {
            from: voter,
            to: vote_state,
            lamports: minimum_balance,
            space: vote_state_space,
            owner: &crate::ID,
        };

        vote_account.invoke()?;

        // Initialize vote state
        let vote_state_data = VoteState::from_account_info(vote_state)?;
        vote_state_data.has_permission = true;
        vote_state_data.vote_count = 1;
        vote_state_data.bump = bump;

    } else {
        // Update existing vote state
        let vote_state_data = VoteState::from_account_info(vote_state)?;

        if !vote_state_data.has_permission {
            return Err(ProgramError::InvalidAccountData);
        };

        // Check if already voted (assuming we want to allow vote changes)
        if vote_state_data.votes[voter_index] != 0 {
            log!("Voter has already voted");
            return Err(ProgramError::InvalidAccountData);
        };

        vote_state_data.vote_count += 1;
    }

    let proposal_data = ProposalState::from_account_info(proposal_state)?;
    proposal_data.votes[voter_index] = vote_choice;

    let multisig_config_data = MultisigConfig::from_account_info(multisig_config)?;
    let mut for_votes = 0;
    let mut against_votes = 0;
    let mut abstain_votes = 0;
    let mut total_votes = 0;

    let active_member_count = multisig_data.num_members.min(10) as usize; // Adjust size as needed

    for i in 0..active_member_count {
        match proposal_data.votes[i] {
            1 => {
                for_votes += 1;
                total_votes += 1;
            },
            2 => {
                against_votes += 1;
                total_votes += 1;
            },
            3 => {
                abstain_votes += 1;
                total_votes += 1;
            },
            _ => {}, // Not voted
        }
    }

    log!("Vote counts : For: {}, Against: {}, Abstain: {}, Total: {}", for_votes, against_votes, abstain_votes, total_votes);

    //Check if proposal should succeed or fail

    if for_votes >= multisig_config_data.min_threshold {
        proposal_data.result = ProposalStatus::Succeeded;
        log!("Proposal succeeded");
    } else if against_votes >= multisig_config_data.min_threshold {
        proposal_data.result = ProposalStatus::Failed;
        log!("Proposal failed");
    } else if current_time > proposal_data.expiry {
        proposal_data.result = ProposalStatus::Cancelled;
        log!("Proposal cancelled due to expiry");
    } else {
        proposal_data.result = ProposalStatus::Active;
        log!("Proposal remains active");
    }

    log!("Vote processed successfully for user: {}", voter.key());

    Ok(())
}

// -------------------------- TESTING -----------------------------

#[cfg(test)]
mod testing_process_vote_instruction {
    use solana_sdk::native_token::LAMPORTS_PER_SOL;

    use super::*;
    use {
        mollusk_svm::{program, Mollusk, result::Check},
        solana_sdk::{
            account::Account,
            pubkey::Pubkey,
            instruction::AccountMeta,
            pubkey,
            instruction::Instruction,
            program_error::ProgramError,
        }
    };

    const ID: Pubkey = pubkey!("4ibrEMW5F6hKnkW4jVedswYv6H6VtwPN6ar6dvXDN1nT");
    const USER: Pubkey = Pubkey::new_from_array([0x01; 32]);
    const MULTISIG: Pubkey = Pubkey::new_from_array([0x02; 32]);

    #[test]
    fn test_process_vote_instruction() {
        println!("STARTING VOTE INSTRUCTION TEST");
        
        let mollusk = Mollusk::new(&ID, "target/deploy/pinocchio_multisig");

        let proposal_id = 12345u64;
        println!("Proposal ID: {}", proposal_id);
        
        let (proposal_state_pda, proposal_bump) = Pubkey::find_program_address(
            &[b"proposal", MULTISIG.as_ref(), &proposal_id.to_le_bytes()],
            &ID,
        );
        println!("Proposal PDA: {}, Bump: {}", proposal_state_pda, proposal_bump);

        let (vote_state_pda, vote_bump) = Pubkey::find_program_address(
            &[b"vote_state", MULTISIG.as_ref(), &proposal_id.to_le_bytes(), &[proposal_bump]],
            &ID,
        );
        println!("Vote State PDA: {}, Bump: {}", vote_state_pda, vote_bump);

        let (multisig_config_pda, _config_bump) = Pubkey::find_program_address(
            &[b"multisig_config", MULTISIG.as_ref()],
            &ID,
        );
        println!("Multisig Config PDA: {}", multisig_config_pda);

        let (system_program_id, system_account) = program::keyed_account_for_system_program();

        let user_account = Account::new(1 * LAMPORTS_PER_SOL, 0, &system_program_id);
        println!("USER ACCOUNT");
        println!("User pubkey: {}", USER);
        println!("User lamports: {}", user_account.lamports);
        println!("User owner: {}", user_account.owner);
        
        let mut multisig_data = vec![0u8; Multisig::LEN];
        multisig_data[0] = 2; 
        multisig_data[1..33].copy_from_slice(USER.as_ref()); 

        let dummy_member = Pubkey::new_unique();
        multisig_data[33..65].copy_from_slice(dummy_member.as_ref());
        let multisig_account = Account::new_data(
            1 * LAMPORTS_PER_SOL,
            &multisig_data,
            &ID,
        ).unwrap();
        
        println!("MULTISIG ACCOUNT DATA");
        println!("Multisig pubkey: {}", MULTISIG);
        println!("Multisig owner: {}", multisig_account.owner);
        println!("Multisig lamports: {}", multisig_account.lamports);
        println!("Multisig data length: {}", multisig_account.data.len());
        println!("Number of members: {}", multisig_data[0]);
        
        for i in 0..2 {
            let start_idx = 1 + (i * 32);
            let end_idx = start_idx + 32;
            let member_bytes = &multisig_data[start_idx..end_idx];
            let member_pubkey = Pubkey::try_from(member_bytes).unwrap();
            println!("Member {}: {}", i, member_pubkey);
        }

        let mut proposal_data = vec![0u8; ProposalState::LEN];
        proposal_data[0..8].copy_from_slice(&proposal_id.to_le_bytes()); 
        proposal_data[8] = 0; 
        
        let future_time = 9999999999u64;
        proposal_data[16..24].copy_from_slice(&future_time.to_le_bytes());
        
        let active_members_offset = 50; 
        proposal_data[active_members_offset..active_members_offset + 32]
            .copy_from_slice(USER.as_ref());
            
        let proposal_state_account = Account::new_data(
            1 * LAMPORTS_PER_SOL,
            &proposal_data,
            &ID,
        ).unwrap();
        
        println!("PROPOSAL STATE ACCOUNT DATA");
        println!("Proposal state pubkey: {}", proposal_state_pda);
        println!("Proposal state owner: {}", proposal_state_account.owner);
        println!("Proposal state lamports: {}", proposal_state_account.lamports);
        println!("Proposal state data length: {}", proposal_state_account.data.len());
        
        let stored_proposal_id = u64::from_le_bytes(proposal_data[0..8].try_into().unwrap());
        let stored_status = proposal_data[8];
        let stored_expiry = u64::from_le_bytes(proposal_data[16..24].try_into().unwrap());

        println!("Stored proposal ID: {}", stored_proposal_id);
        println!("Stored proposal status: {}", stored_status);
        println!("Stored proposal expiry: {}", stored_expiry);
        
        let active_member_bytes = &proposal_data[active_members_offset..active_members_offset + 32];
        let active_member = Pubkey::try_from(active_member_bytes).unwrap();
        println!("Active member: {}", active_member);

        let vote_state_account = Account::new(0, 0, &system_program_id);
        println!("=== VOTE STATE ACCOUNT (BEFORE) ===");
        println!("Vote state pubkey: {}", vote_state_pda);
        println!("Vote state owner: {}", vote_state_account.owner);
        println!("Vote state lamports: {}", vote_state_account.lamports);
        println!("Vote state data length: {}", vote_state_account.data.len());

        let mut multisig_config_data = vec![0u8; MultisigConfig::LEN];
        multisig_config_data[0..8].copy_from_slice(&1u64.to_le_bytes());
        let multisig_config_account = Account::new_data(
            1 * LAMPORTS_PER_SOL,
            &multisig_config_data,
            &ID,
        ).unwrap();
        
        println!("MULTISIG CONFIG ACCOUNT DATA");
        println!("Config pubkey: {}", multisig_config_pda);
        println!("Config owner: {}", multisig_config_account.owner);
        println!("Config lamports: {}", multisig_config_account.lamports);
        println!("Config data length: {}", multisig_config_account.data.len());
        
        let min_threshold = u64::from_le_bytes(multisig_config_data[0..8].try_into().unwrap());
        println!("Min threshold: {}", min_threshold);

        let ix_accounts = vec![
            AccountMeta::new(USER, true),                    // voter (signer)
            AccountMeta::new(MULTISIG, false),               // multisig
            AccountMeta::new(proposal_state_pda, false),     // proposal_state
            AccountMeta::new(vote_state_pda, false),         // vote_state
            AccountMeta::new(multisig_config_pda, false),    // multisig_config
            AccountMeta::new_readonly(system_program_id, false), // system_program
        ];

        let mut data = vec![1u8]; // Instruction discriminator for vote
        data.extend_from_slice(&proposal_id.to_le_bytes()); 
        data.push(1); // Vote choice (1(dor))
        data.push(proposal_bump); 

        println!("INSTRUCTION DATA");
        println!("Instruction discriminator: {}", data[0]);
        println!("Instruction data length: {}", data.len());
        println!("Vote choice: {}", data[9]); 
        println!("Bump used: {}", data[10]); 

        // Create the instruction
        let instruction = Instruction::new_with_bytes(ID, &data, ix_accounts);

        // Prepare transaction accounts
        let tx_accounts = vec![
            (USER, user_account),
            (MULTISIG, multisig_account),
            (proposal_state_pda, proposal_state_account),
            (vote_state_pda, vote_state_account),
            (multisig_config_pda, multisig_config_account),
            (system_program_id, system_account),
        ];

        println!("PROCESSING INSTRUCTION");
        
        // Process and validate the instruction
        mollusk.process_and_validate_instruction(
            &instruction,
            &tx_accounts,
            &[Check::success()],
        );

        println!("INSTRUCTION PROCESSING COMPLETE");
        println!("TEST COMPLETE");
    }

    #[test]
    fn test_vote_instruction_wrong_program_owner() {
        println!("Testing: Wrong Program Owner");
        println!("This test verifies that the contract rejects multisig accounts not owned by the correct program");
        
        let mollusk = Mollusk::new(&ID, "target/deploy/pinocchio_multisig");
        let proposal_id = 12345u64;
        
        let (proposal_state_pda, proposal_bump) = Pubkey::find_program_address(
            &[b"proposal", MULTISIG.as_ref(), &proposal_id.to_le_bytes()],
            &ID,
        );
        println!("Proposal PDA: {}, Bump: {}", proposal_state_pda, proposal_bump);

        let (vote_state_pda, vote_bump) = Pubkey::find_program_address(
            &[b"vote_state", MULTISIG.as_ref(), &proposal_id.to_le_bytes(), &[proposal_bump]],
            &ID,
        );
        println!("Vote State PDA: {}, Bump: {}", vote_state_pda, vote_bump);

        let (multisig_config_pda, _config_bump) = Pubkey::find_program_address(
            &[b"multisig_config", MULTISIG.as_ref()],
            &ID,
        );
        println!("Multisig Config PDA: {}", multisig_config_pda);

        let (system_program_id, system_account) = program::keyed_account_for_system_program();

        let user_account = Account::new(1 * LAMPORTS_PER_SOL, 0, &system_program_id);
        println!("User Account - Pubkey: {}, Lamports: {}", USER, user_account.lamports);

        let mut multisig_data = vec![0u8; Multisig::LEN];
        multisig_data[0] = 2;
        multisig_data[1..33].copy_from_slice(USER.as_ref());
        let dummy_member = Pubkey::new_unique();
        multisig_data[33..65].copy_from_slice(dummy_member.as_ref()); 
        
        let wrong_owner = Pubkey::new_unique(); 
        let multisig_account = Account::new_data(
            1 * LAMPORTS_PER_SOL, 
            &multisig_data, 
            &wrong_owner // This should be &ID, but we're using wrong_owner to test failure
        ).unwrap();
        
        println!("Multisig Account - Expected Owner: {}, Actual Owner: {}", ID, wrong_owner);
        println!("Multisig Account - Pubkey: {}, Lamports: {}", MULTISIG, multisig_account.lamports);
        println!("Multisig Members: {} (count: {})", USER, multisig_data[0]);
        
        // Create valid proposal account (owned by correct program)
        let mut proposal_data = vec![0u8; ProposalState::LEN];
        proposal_data[0..8].copy_from_slice(&proposal_id.to_le_bytes()); // proposal_id
        proposal_data[8] = 0; // status = Active (ProposalStatus::Active)
        let future_time = 9999999999u64; // Far future expiry
        proposal_data[16..24].copy_from_slice(&future_time.to_le_bytes());
        
        // Set active members - USER is an active member
        let active_members_offset = 50; 
        proposal_data[active_members_offset..active_members_offset + 32]
            .copy_from_slice(USER.as_ref());
            
        let proposal_state_account = Account::new_data(
            1 * LAMPORTS_PER_SOL,
            &proposal_data,
            &ID, // Correctly owned by our program
        ).unwrap();
        
        println!("Proposal Account - Owner: {}, Proposal ID: {}", proposal_state_account.owner, proposal_id);
        
        // Create empty vote state account (will be created during instruction)
        let vote_state_account = Account::new(0, 0, &system_program_id);
        println!("Vote State Account - Initial Owner: {}, Lamports: {}", vote_state_account.owner, vote_state_account.lamports);

        // Create valid multisig config account
        let mut multisig_config_data = vec![0u8; MultisigConfig::LEN];
        multisig_config_data[0..8].copy_from_slice(&1u64.to_le_bytes()); // min_threshold = 1
        let multisig_config_account = Account::new_data(
            1 * LAMPORTS_PER_SOL,
            &multisig_config_data,
            &ID, // Correctly owned by our program
        ).unwrap();
        
        println!("Multisig Config Account - Owner: {}, Threshold: {}", multisig_config_account.owner, 1);

        // Set up instruction accounts
        let ix_accounts = vec![
            AccountMeta::new(USER, true),                    // voter (signer) - MUST BE SIGNER
            AccountMeta::new(MULTISIG, false),               // multisig (WRONG OWNER - should fail)
            AccountMeta::new(proposal_state_pda, false),     // proposal_state
            AccountMeta::new(vote_state_pda, false),         // vote_state
            AccountMeta::new(multisig_config_pda, false),    // multisig_config
            AccountMeta::new_readonly(system_program_id, false), // system_program
        ];

        // Create instruction data
        let mut data = vec![1u8]; // Instruction discriminator for vote
        data.extend_from_slice(&proposal_id.to_le_bytes()); // proposal_id (8 bytes)
        data.push(1); // vote_choice = 1 (For)
        data.push(proposal_bump); // bump for PDA derivation

        println!("Instruction Data:");
        println!("  - Discriminator: {}", data[0]);
        println!("  - Proposal ID: {}", proposal_id);
        println!("  - Vote Choice: {} (1=For)", data[9]);
        println!("  - Bump: {}", data[10]);
        println!("  - Total Data Length: {}", data.len());

        // Create the instruction
        let instruction = Instruction::new_with_bytes(ID, &data, ix_accounts);

        // Prepare transaction accounts
        let tx_accounts = vec![
            (USER, user_account),
            (MULTISIG, multisig_account),                    // This account has WRONG OWNER
            (proposal_state_pda, proposal_state_account),
            (vote_state_pda, vote_state_account),
            (multisig_config_pda, multisig_config_account),
            (system_program_id, system_account),
        ];

        println!("Processing instruction - expecting failure due to wrong multisig owner...");
        
        // Process and validate the instruction - should fail with IncorrectProgramId
        mollusk.process_and_validate_instruction(
            &instruction,
            &tx_accounts,
            &[Check::err(ProgramError::IncorrectProgramId)],
        );

        println!("✓ TEST PASSED: Contract correctly rejected multisig account with wrong owner");
        println!("✓ Expected Error: ProgramError::IncorrectProgramId");
        println!("✓ This confirms the security check: 'if multisig.owner() != &crate::ID' works correctly");
        println!("=== Test Complete ===");
    }

}